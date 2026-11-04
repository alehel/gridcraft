//! OpenDocument Spreadsheet import (ODF 1.3 part 3 §9): sheets, typed values, formulas, cell
//! styles and number formats, merges, column widths and row heights, outlines, notes, links,
//! named ranges, data validation, conditional formatting, autofilters, pictures, frozen panes,
//! sheet settings and document properties.

use std::collections::HashMap;
use std::sync::Arc;

use gridcraft_core::date::{days_in_month, serial_from_ymd};
use gridcraft_core::{CellError, CellRef, DateSystem, MAX_COLS, MAX_ROWS, RangeRef, Value};
use gridcraft_model::{
    Anchor, AutoFilter, Cell, CfOperator, CfRule, CfValueKind, Color, Comment, CondFormat, DefinedName, ErrorStyle, Font, Formula, Hyperlink, Image,
    LineInfo, NumFmt, Sheet, SheetProtection, Style, StyleId, StyleTable, Validation, ValidationKind, Visibility, Workbook,
};

use super::dom::{self, Elem, Ns, Sink};
use super::numfmt::{self, DataStyle};
use super::style::{self, CellStyleDef, Props, color, length_pt, pt_to_px};
use super::{formula, settings};
use crate::package::{MAX_ENTRIES, Package};
use crate::{IoError, ReadReport};

const MIME: &[u8] = b"application/vnd.oasis.opendocument.spreadsheet";
const TEMPLATE_MIME: &[u8] = b"application/vnd.oasis.opendocument.spreadsheet-template";
const MAX_CELLS: u64 = 4_000_000;
const MAX_STYLED_BLANKS: u64 = 2_000_000;
const MAX_MERGES: u64 = 100_000;
const MAX_TEXT: u64 = 256 * 1024 * 1024;
const MAX_SHEETS: usize = 1024;
const MAX_LINE_ENTRIES: usize = 200_000;
const MAX_STYLE_DEPTH: usize = 32;
/// LibreOffice's defaults for a document that states none.
pub(crate) const ODS_FONT: &str = "Liberation Sans";
pub(crate) const ODS_FONT_SIZE: f32 = 10.0;

/// Reads only the small, exact mimetype entry (a spreadsheet or a spreadsheet template),
/// including when the ZIP uses compression.
pub(crate) fn has_mimetype(bytes: &[u8]) -> bool {
    use std::io::Read;
    let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else { return false };
    if zip.len() > MAX_ENTRIES {
        return false;
    }
    let Ok(mut file) = zip.by_name("mimetype") else { return false };
    if file.size() != MIME.len() as u64 && file.size() != TEMPLATE_MIME.len() as u64 {
        return false;
    }
    let mut value = Vec::new();
    (&mut file).take(TEMPLATE_MIME.len() as u64 + 1).read_to_end(&mut value).is_ok() && (value == MIME || value == TEMPLATE_MIME)
}

fn xml_error(e: impl std::fmt::Display) -> IoError {
    IoError::Xml(format!("ODS: {e}"))
}

fn check_manifest(bytes: &[u8]) -> Result<(), IoError> {
    let root = dom::parse_tree(bytes)?;
    if !root.is(Ns::Manifest, "manifest") {
        return Err(xml_error("expected one manifest:manifest root"));
    }
    if !root.descendants(Ns::Manifest, "encryption-data").is_empty() {
        return Err(IoError::Format("encrypted ODS files are not supported".into()));
    }
    Ok(())
}

/// Imports an ODS package. Always inspect the returned warnings.
pub fn read_ods(bytes: &[u8]) -> Result<(Workbook, ReadReport), IoError> {
    let mut package = Package::open(bytes)?;
    if !has_mimetype(bytes) {
        return Err(IoError::Format("missing or invalid ODS mimetype".into()));
    }
    if let Some(manifest) = package.read("META-INF/manifest.xml")? {
        check_manifest(&manifest)?;
    }
    let content = package.read("content.xml")?.ok_or_else(|| IoError::Format("ODS content.xml is missing".into()))?;
    let mut import = Import::new();
    if let Some(styles) = package.read("styles.xml")? {
        match dom::parse_tree(&styles) {
            Ok(tree) => import.read_styles_part(&tree),
            Err(_) => import.report.warn("The ODS styles part is damaged; default styles were used."),
        }
    }
    dom::parse(&content, &mut import)?;
    if !import.spreadsheet_seen {
        return Err(xml_error("incomplete ODS document"));
    }
    if import.wb.sheets.is_empty() {
        return Err(IoError::Format("ODS contains no worksheets".into()));
    }
    import.finish_names();
    if let Some(s) = package.read("settings.xml")?
        && let Ok(tree) = dom::parse_tree(&s)
    {
        settings::apply(&tree, &mut import.wb);
    }
    if let Some(m) = package.read("meta.xml")?
        && let Ok(tree) = dom::parse_tree(&m)
    {
        read_meta(&tree, &mut import.wb);
    }
    import.load_pictures(&mut package);
    Ok((import.wb, import.report))
}

fn read_meta(root: &Elem, wb: &mut Workbook) {
    let Some(meta) = root.child(Ns::Office, "meta") else { return };
    let text = |ns: Ns, name: &str| meta.child(ns, name).map(Elem::raw_text);
    if let Some(t) = text(Ns::Dc, "title") {
        wb.props.title = t;
    }
    if let Some(t) = text(Ns::Dc, "subject") {
        wb.props.subject = t;
    }
    if let Some(t) = text(Ns::Dc, "description") {
        wb.props.description = t;
    }
    if let Some(t) = text(Ns::Meta, "initial-creator").or_else(|| text(Ns::Dc, "creator")) {
        wb.props.author = t;
    }
    let keywords: Vec<String> = meta.children(Ns::Meta, "keyword").map(Elem::raw_text).collect();
    if !keywords.is_empty() {
        wb.props.keywords = keywords.join(", ");
    }
}

#[derive(Clone, Debug, Default)]
struct ColStyle {
    width: Option<f32>,
    page_break: bool,
}
#[derive(Clone, Debug, Default)]
struct RowStyle {
    height: Option<f32>,
    optimal: bool,
    page_break: bool,
}
#[derive(Clone, Debug, Default)]
struct TableStyle {
    hidden: bool,
    tab_color: Option<Color>,
    rtl: bool,
    master_page: Option<String>,
}

/// A run of identical columns or rows as read.
#[derive(Clone, Debug)]
struct LineRun {
    start: u32,
    count: u32,
    size: Option<f32>,
    optimal: bool,
    hidden: bool,
    outline: u8,
    collapsed: bool,
    style: Option<StyleId>,
    page_break: bool,
}

struct SheetState {
    sheet: Sheet,
    depth: usize,
    row: u32,
    col: u32,
    cols: Vec<LineRun>,
    rows: Vec<LineRun>,
    row_level: u8,
    col_level: u8,
    /// Group levels hidden (`table:display="false"`) at each depth.
    row_group_hidden: Vec<bool>,
    col_group_hidden: Vec<bool>,
    header_rows: Option<(u32, u32)>,
    header_cols: Option<(u32, u32)>,
    in_header_rows: bool,
    in_header_cols: bool,
    validation_cells: HashMap<String, Vec<RangeRef>>,
    last_content_row: Option<u32>,
    print_ranges: Option<String>,
    frames: Vec<Frame>,
    /// A cached copy of another document's sheet (an external reference's source), not a sheet.
    external: bool,
    /// Cells whose style carries `style:map` conditions, by style name.
    mapped: HashMap<String, Vec<RangeRef>>,
    has_calcext_formats: bool,
}

/// A picture as read, before its anchor is placed and its bytes loaded.
struct Frame {
    href: String,
    /// An embedded object (a chart) rather than a picture.
    object: bool,
    cell: Option<CellRef>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    alt: String,
}

struct ValidationDef {
    v: Validation,
}

/// A defined name as read: name, scope, ODF expression or range, whether a range, base cell.
type NameDef = (String, Option<usize>, String, bool, Option<String>);

/// A cell as a row holds it before it is placed: column, repeat, width and height spanned, the
/// cell, and its element (for notes, links and pictures).
type RowCell<'a> = (u32, u32, u32, u32, Cell, Option<&'a Elem>);

pub(crate) struct Import {
    pub wb: Workbook,
    pub report: ReadReport,
    fonts: HashMap<String, String>,
    data_styles: HashMap<String, DataStyle>,
    named: HashMap<String, CellStyleDef>,
    auto: HashMap<String, CellStyleDef>,
    default_props: Props,
    ids: HashMap<String, StyleId>,
    col_styles: HashMap<String, ColStyle>,
    row_styles: HashMap<String, RowStyle>,
    table_styles: HashMap<String, TableStyle>,
    page_layouts: HashMap<String, Elem>,
    master_pages: HashMap<String, Elem>,
    validations: HashMap<String, ValidationDef>,
    names: Vec<NameDef>,
    database_ranges: Vec<Elem>,
    sheet: Option<SheetState>,
    styles_ready: bool,
    pub spreadsheet_seen: bool,
    depth: usize,
    cells: u64,
    blanks: u64,
    merges: u64,
    text: u64,
    images: Vec<(usize, Frame)>,
}

fn attr_u32(e: &Elem, ns: Ns, name: &str, default: u32, limit: u32) -> Result<u32, IoError> {
    let n = match e.attr(ns, name) {
        Some(s) => s.trim().parse::<u32>().map_err(|_| IoError::Format("invalid ODS repeat or span count".into()))?,
        None => default,
    };
    if n == 0 || n > limit {
        return Err(IoError::TooLarge("ODS repeat or span exceeds worksheet bounds".into()));
    }
    Ok(n)
}

impl Import {
    fn new() -> Self {
        let mut wb = Workbook::new();
        wb.sheets.clear();
        Import {
            wb,
            report: ReadReport::default(),
            fonts: HashMap::new(),
            data_styles: HashMap::new(),
            named: HashMap::new(),
            auto: HashMap::new(),
            default_props: Props::default(),
            ids: HashMap::new(),
            col_styles: HashMap::new(),
            row_styles: HashMap::new(),
            table_styles: HashMap::new(),
            page_layouts: HashMap::new(),
            master_pages: HashMap::new(),
            validations: HashMap::new(),
            names: Vec::new(),
            database_ranges: Vec::new(),
            sheet: None,
            styles_ready: false,
            spreadsheet_seen: false,
            depth: 0,
            cells: 0,
            blanks: 0,
            merges: 0,
            text: 0,
            images: Vec::new(),
        }
    }

    fn read_fonts(&mut self, decls: &Elem) {
        for f in decls.children(Ns::Style, "font-face") {
            if let Some(name) = f.attr(Ns::Style, "name") {
                let family = f.attr(Ns::Svg, "font-family").unwrap_or(name).trim().trim_matches(|c| c == '\'' || c == '"').to_string();
                self.fonts.insert(name.to_string(), family);
            }
        }
    }

    /// Reads a style container (`office:styles`, `office:automatic-styles`); `named` for common styles.
    fn read_style_list(&mut self, list: &Elem, named: bool) {
        numfmt::read_styles(list, &mut self.data_styles);
        for e in list.elems() {
            if e.is(Ns::Style, "default-style") && e.attr(Ns::Style, "family") == Some("table-cell") {
                self.default_props = style::read_props(e, &self.fonts);
                continue;
            }
            if e.is(Ns::Style, "page-layout") {
                if let Some(n) = e.attr(Ns::Style, "name") {
                    self.page_layouts.insert(n.to_string(), e.clone());
                }
                continue;
            }
            if !e.is(Ns::Style, "style") {
                continue;
            }
            let Some(name) = e.attr(Ns::Style, "name") else { continue };
            match e.attr(Ns::Style, "family") {
                Some("table-cell") => {
                    let maps = e
                        .children(Ns::Style, "map")
                        .filter_map(|m| {
                            Some((
                                m.attr(Ns::Style, "condition")?.to_string(),
                                m.attr(Ns::Style, "apply-style-name")?.to_string(),
                                m.attr(Ns::Style, "base-cell-address").map(str::to_string),
                            ))
                        })
                        .collect();
                    let def = CellStyleDef {
                        parent: e.attr(Ns::Style, "parent-style-name").map(str::to_string),
                        props: style::read_props(e, &self.fonts),
                        maps,
                    };
                    if named {
                        self.named.insert(name.to_string(), def.clone());
                        if let Some(display) = e.attr(Ns::Style, "display-name") {
                            self.named.entry(display.to_string()).or_insert(def);
                        }
                    } else {
                        self.auto.insert(name.to_string(), def);
                    }
                }
                Some("table-column") => {
                    let p = e.child(Ns::Style, "table-column-properties");
                    let width = p.and_then(|p| p.attr(Ns::Style, "column-width")).and_then(length_pt).map(pt_to_px);
                    let page_break = p.and_then(|p| p.attr(Ns::Fo, "break-before")) == Some("page");
                    self.col_styles.insert(name.to_string(), ColStyle { width, page_break });
                }
                Some("table-row") => {
                    let p = e.child(Ns::Style, "table-row-properties");
                    let height = p
                        .and_then(|p| p.attr(Ns::Style, "row-height").or_else(|| p.attr(Ns::Style, "min-row-height")))
                        .and_then(length_pt)
                        .map(pt_to_px);
                    let optimal = p.and_then(|p| p.attr(Ns::Style, "use-optimal-row-height")) == Some("true");
                    let page_break = p.and_then(|p| p.attr(Ns::Fo, "break-before")) == Some("page");
                    self.row_styles.insert(name.to_string(), RowStyle { height, optimal, page_break });
                }
                Some("table") => {
                    let p = e.child(Ns::Style, "table-properties");
                    let hidden = p.and_then(|p| p.attr(Ns::Table, "display")) == Some("false");
                    let tab_color = p.and_then(|p| p.attr(Ns::TableOoo, "tab-color").or_else(|| p.attr(Ns::Table, "tab-color"))).and_then(color);
                    let rtl = p.and_then(|p| p.attr(Ns::Style, "writing-mode")).is_some_and(|w| w.starts_with("rl"));
                    let master_page = e.attr(Ns::Style, "master-page-name").map(str::to_string);
                    self.table_styles.insert(name.to_string(), TableStyle { hidden, tab_color, rtl, master_page });
                }
                _ => {}
            }
        }
    }

    pub(crate) fn read_styles_part(&mut self, root: &Elem) {
        if let Some(f) = root.child(Ns::Office, "font-face-decls") {
            self.read_fonts(f);
        }
        if let Some(s) = root.child(Ns::Office, "styles") {
            self.read_style_list(s, true);
        }
        if let Some(s) = root.child(Ns::Office, "automatic-styles") {
            self.read_style_list(s, false);
        }
        if let Some(m) = root.child(Ns::Office, "master-styles") {
            for p in m.children(Ns::Style, "master-page") {
                if let Some(n) = p.attr(Ns::Style, "name") {
                    self.master_pages.insert(n.to_string(), p.clone());
                }
            }
        }
    }

    /// The properties of a named style with its ancestors.
    fn resolve_named(&self, name: &str) -> Props {
        let mut chain: Vec<&Props> = Vec::new();
        let mut cur = Some(name.to_string());
        while let Some(n) = cur.take() {
            if chain.len() >= MAX_STYLE_DEPTH {
                break;
            }
            let Some(def) = self.named.get(&n) else { break };
            chain.push(&def.props);
            cur = def.parent.clone();
        }
        let mut p = self.default_props.clone();
        for c in chain.iter().rev() {
            p = style::merge(&p, c);
        }
        p
    }

    fn base_style(&self) -> Style {
        let p = self.resolve_named("Default");
        let base = Style { font: Font { name: ODS_FONT.into(), size: ODS_FONT_SIZE, minor: false, ..Font::default() }, ..Style::default() };
        let code = p.data_style.as_deref().and_then(|d| numfmt::code(&self.data_styles, d));
        style::to_style(&p, &base, code)
    }

    /// Called once the styles are known: makes the workbook's default style the file's Default.
    fn ready_styles(&mut self) {
        if self.styles_ready {
            return;
        }
        self.styles_ready = true;
        let base = self.base_style();
        self.wb.styles = StyleTable::with_default(base);
    }

    /// The style id of a cell style name (automatic, or named when no automatic style has it).
    fn style_id(&mut self, name: &str) -> StyleId {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        let props = match self.auto.get(name) {
            Some(def) => {
                let parent = def.parent.clone().unwrap_or_else(|| "Default".into());
                style::merge(&self.resolve_named(&parent), &def.props)
            }
            None => self.resolve_named(name),
        };
        let code = props.data_style.as_deref().and_then(|d| numfmt::code(&self.data_styles, d));
        let base = self.wb.styles.get(StyleId::DEFAULT).clone();
        let base_default = Style { font: Font { name: ODS_FONT.into(), size: ODS_FONT_SIZE, minor: false, ..Font::default() }, ..Style::default() };
        // Resolve on top of the plain defaults so that a property set back to the default sticks.
        let mut s = style::to_style(&props, &base_default, code.clone());
        if code.is_none() {
            s.num_fmt = base.num_fmt.clone();
        }
        let id = self.wb.styles.intern(s);
        self.ids.insert(name.to_string(), id);
        id
    }

    /// A style that looks exactly like the default one but is stored apart from it: the model
    /// drops cells in the default style, so a cell that overrides its row's or column's format with
    /// the default needs it. Only the font family class differs (none and 2 are both swiss).
    fn default_twin(&mut self) -> StyleId {
        self.wb.styles.derive(StyleId::DEFAULT, |s| s.font.family = if s.font.family.is_none() { Some(2) } else { None })
    }

    /// The differential style of a conditional format's named style: only what it states.
    fn dxf(&self, name: &str) -> Style {
        let mut chain: Vec<&Props> = Vec::new();
        let mut cur = Some(name.to_string());
        while let Some(n) = cur.take() {
            if chain.len() >= MAX_STYLE_DEPTH || n == "Default" {
                break;
            }
            let Some(def) = self.named.get(&n).or_else(|| self.auto.get(&n)) else { break };
            chain.push(&def.props);
            cur = def.parent.clone();
        }
        let mut p = Props::default();
        for c in chain.iter().rev() {
            p = style::merge(&p, c);
        }
        let code = p.data_style.as_deref().and_then(|d| numfmt::code(&self.data_styles, d));
        let mut s = style::to_style(&p, &Style::default(), code);
        if p.font_name.is_none() {
            s.font.minor = Font::default().minor;
        }
        s
    }

    fn open_table(&mut self, e: &Elem) -> Result<(), IoError> {
        if self.wb.sheets.len() >= MAX_SHEETS {
            return Err(IoError::TooLarge("ODS contains more than 1024 sheets".into()));
        }
        let original = e.attr(Ns::Table, "name").unwrap_or_default().to_string();
        let mut name = crate::read::SheetNames::of(&self.wb).unique(original.trim_matches('\''), self.wb.sheets.len());
        if self.wb.check_sheet_name(&name, None).is_err() {
            name = self.wb.next_sheet_name();
        }
        if name != original {
            self.report.warn("Some ODS sheet names were shortened or renamed to valid, unique sheet names.");
        }
        let mut sheet = Sheet::new(name);
        if let Some(ts) = e.attr(Ns::Table, "style-name").and_then(|n| self.table_styles.get(n)).cloned() {
            if ts.hidden {
                sheet.visibility = Visibility::Hidden;
            }
            sheet.tab_color = ts.tab_color;
            sheet.right_to_left = ts.rtl;
            super::page::apply(&mut sheet, ts.master_page.as_deref().unwrap_or("Default"), &self.master_pages, &self.page_layouts);
        }
        if e.attr(Ns::Table, "style-name").is_none_or(|n| !self.table_styles.contains_key(n)) {
            super::page::apply(&mut sheet, "Default", &self.master_pages, &self.page_layouts);
        }
        if e.attr(Ns::Table, "protected") == Some("true") {
            sheet.protection = Some(SheetProtection { select_locked: true, select_unlocked: true, ..SheetProtection::default() });
        }
        if e.attr(Ns::Table, "print") == Some("false") {
            sheet.print.print_area = None;
        }
        self.sheet = Some(SheetState {
            sheet,
            depth: self.depth,
            row: 0,
            col: 0,
            cols: Vec::new(),
            rows: Vec::new(),
            row_level: 0,
            col_level: 0,
            row_group_hidden: Vec::new(),
            col_group_hidden: Vec::new(),
            header_rows: None,
            header_cols: None,
            in_header_rows: false,
            in_header_cols: false,
            validation_cells: HashMap::new(),
            last_content_row: None,
            print_ranges: e.attr(Ns::Table, "print-ranges").map(str::to_string),
            frames: Vec::new(),
            external: original.starts_with('\'') && original.contains("'#"),
            mapped: HashMap::new(),
            has_calcext_formats: false,
        });
        Ok(())
    }

    fn column(&mut self, e: &Elem) -> Result<(), IoError> {
        let repeat = attr_u32(e, Ns::Table, "number-columns-repeated", 1, u32::MAX)?;
        let style = e.attr(Ns::Table, "style-name").and_then(|n| self.col_styles.get(n)).cloned().unwrap_or_default();
        let default_style = match e.attr(Ns::Table, "default-cell-style-name") {
            Some(n) if n != "Default" => Some(self.style_id(n)),
            _ => None,
        };
        let hidden = matches!(e.attr(Ns::Table, "visibility"), Some("collapse" | "filter"));
        let Some(st) = self.sheet.as_mut() else { return Ok(()) };
        let start = st.col;
        let count = repeat.min(MAX_COLS.saturating_sub(start));
        st.col = start.saturating_add(repeat).min(MAX_COLS);
        if count == 0 {
            return Ok(());
        }
        if st.in_header_cols {
            let end = start + count - 1;
            st.header_cols = Some(st.header_cols.map_or((start, end), |(a, _)| (a, end)));
        }
        let collapsed = st.col_group_hidden.last().copied().unwrap_or(false);
        st.cols.push(LineRun {
            start,
            count,
            size: style.width,
            optimal: false,
            hidden,
            outline: st.col_level,
            collapsed: collapsed && hidden,
            style: default_style.filter(|s| *s != StyleId::DEFAULT),
            page_break: style.page_break,
        });
        Ok(())
    }

    fn row(&mut self, e: &Elem) -> Result<(), IoError> {
        let repeat = attr_u32(e, Ns::Table, "number-rows-repeated", 1, u32::MAX)?;
        let rs = e.attr(Ns::Table, "style-name").and_then(|n| self.row_styles.get(n)).cloned().unwrap_or_default();
        let hidden = matches!(e.attr(Ns::Table, "visibility"), Some("collapse" | "filter"));
        let row_default = match e.attr(Ns::Table, "default-cell-style-name") {
            Some(n) if n != "Default" => Some(self.style_id(n)),
            _ => None,
        };
        // The column default styles, for cells without their own style.
        let col_defaults: Vec<(u32, u32, StyleId)> = self
            .sheet
            .as_ref()
            .map(|s| s.cols.iter().filter_map(|c| c.style.map(|st| (c.start, c.start + c.count, st))).collect())
            .unwrap_or_default();
        let Some(start_row) = self.sheet.as_ref().map(|s| s.row) else { return Ok(()) };
        let end = start_row.checked_add(repeat).filter(|n| *n <= MAX_ROWS);
        let mut col = 0u32;
        let mut row_has_content = false;
        let mut kept: Vec<RowCell<'_>> = Vec::new();
        for c in e.elems() {
            let covered = c.is(Ns::Table, "covered-table-cell");
            if !(c.is(Ns::Table, "table-cell") || covered) {
                continue;
            }
            let crepeat = attr_u32(c, Ns::Table, "number-columns-repeated", 1, u32::MAX)?;
            if col >= MAX_COLS {
                if c.attr(Ns::Office, "value-type").is_some() || c.attr(Ns::Table, "formula").is_some() || c.child(Ns::Text, "p").is_some() {
                    self.report.warn("Some ODS columns beyond column XFD were not imported.");
                }
                break;
            }
            let width = if covered { 1 } else { attr_u32(c, Ns::Table, "number-columns-spanned", 1, MAX_COLS)? };
            let height = if covered { 1 } else { attr_u32(c, Ns::Table, "number-rows-spanned", 1, MAX_ROWS)? };
            let converted = self.convert(c, CellRef::new(start_row, col))?;
            let explicit_style = c.attr(Ns::Table, "style-name").map(|n| self.style_id(n));
            let has_extras =
                c.elems().any(|k| k.is(Ns::Office, "annotation") || k.ns == Ns::Draw) || c.attr(Ns::Table, "content-validation-name").is_some();
            let cend = match col.checked_add(crepeat).filter(|n| *n <= MAX_COLS) {
                Some(n) => n,
                None => {
                    if converted.has_content() {
                        self.report.warn("Some ODS columns beyond column XFD were not imported.");
                    }
                    MAX_COLS
                }
            };
            // A repeat spanning columns with different default styles is split at their edges.
            let mut seg = col;
            while seg < cend {
                let col_default = col_defaults.iter().find(|(a, b, _)| seg >= *a && seg < *b);
                let seg_end = match col_default {
                    Some((_, b, _)) => (*b).min(cend),
                    None => col_defaults.iter().map(|(a, _, _)| *a).filter(|a| *a > seg).min().unwrap_or(cend).min(cend),
                };
                let col_style = col_default.map(|(_, _, s)| *s).unwrap_or(StyleId::DEFAULT);
                let mut cell = converted.clone();
                let style = explicit_style.or(row_default).or(col_default.map(|(_, _, s)| *s));
                if let Some(s) = style {
                    // A date or time value keeps a date format when its style has none.
                    cell.style = if cell.style != StyleId::DEFAULT && self.wb.styles.get(s).num_fmt.as_str() == "General" {
                        let code = self.wb.styles.get(cell.style).num_fmt.clone();
                        self.wb.styles.derive(s, |st| st.num_fmt = code)
                    } else {
                        s
                    };
                }
                if cell.formula.is_none() {
                    self.wrap_lines(&mut cell);
                }
                // A cell set back to the default style under a formatted row or column keeps it.
                if cell.style == StyleId::DEFAULT && (row_default.is_some() || col_style != StyleId::DEFAULT) {
                    cell.style = self.default_twin();
                }
                // Blank cells that only repeat the row's or column's default style need no entry.
                let interesting =
                    cell.has_content() || (cell.style != col_style && Some(cell.style) != row_default) || width > 1 || height > 1 || has_extras;
                if interesting {
                    if cell.has_content() {
                        row_has_content = true;
                    }
                    kept.push((seg, seg_end - seg, width, height, cell, Some(c)));
                }
                seg = seg_end;
            }
            col = cend;
        }
        if start_row >= MAX_ROWS {
            if kept.iter().any(|k| k.4.has_content()) {
                self.report.warn("Some ODS rows beyond row 1,048,576 were not imported.");
            }
            return Ok(());
        }
        let end = match end {
            Some(e) => e,
            None => {
                if kept.iter().any(|k| k.4.has_content()) {
                    self.report.warn("Some ODS rows beyond row 1,048,576 were not imported.");
                }
                MAX_ROWS
            }
        };
        let rows = end - start_row;
        for (col, count, width, height, cell, el) in kept {
            // Long runs of empty cells whose format shows nothing (alignment, fonts, number
            // formats) are what spreadsheet writers emit for whole-column formatting; storing a
            // million of them would cost far more than it shows.
            if rows > 1000
                && !cell.has_content()
                && width == 1
                && height == 1
                && !el.is_some_and(|e| e.elems().next().is_some() || e.attr(Ns::Table, "content-validation-name").is_some())
            {
                let st = self.wb.styles.get(cell.style);
                let b = &st.borders;
                if st.fill.pattern == gridcraft_model::PatternType::None
                    && [b.left, b.right, b.top, b.bottom, b.diag_down, b.diag_up].iter().all(|l| l.is_none())
                {
                    continue;
                }
            }
            self.place(start_row, rows, col, count, width, height, cell, el)?;
        }
        let Some(st) = self.sheet.as_mut() else { return Ok(()) };
        if row_has_content {
            st.last_content_row = Some(end - 1);
        }
        if rows > 0 {
            if st.in_header_rows {
                st.header_rows = Some(st.header_rows.map_or((start_row, end - 1), |(a, _)| (a, end - 1)));
            }
            let collapsed = st.row_group_hidden.last().copied().unwrap_or(false);
            st.rows.push(LineRun {
                start: start_row,
                count: rows,
                size: rs.height,
                optimal: rs.optimal,
                hidden,
                outline: st.row_level,
                collapsed: collapsed && hidden,
                style: row_default,
                page_break: rs.page_break,
            });
        }
        st.row = end;
        Ok(())
    }

    /// Stores a (repeated) cell and what hangs off it: merges, notes, links, validation, pictures.
    #[allow(clippy::too_many_arguments)]
    fn place(&mut self, row: u32, rows: u32, col: u32, cols: u32, width: u32, height: u32, cell: Cell, el: Option<&Elem>) -> Result<(), IoError> {
        let count = u64::from(rows) * u64::from(cols);
        if cell.has_content() {
            self.cells = self.cells.saturating_add(count);
            if let Value::Text(t) = &cell.value {
                self.text = self.text.saturating_add(count.saturating_mul(t.len() as u64));
            }
        } else {
            self.blanks = self.blanks.saturating_add(count);
            if self.blanks > MAX_STYLED_BLANKS {
                self.report.warn("Some ODS formatting of empty cells beyond the import limit was not imported.");
                return Ok(());
            }
        }
        let merged = width > 1 || height > 1;
        if merged {
            self.merges = self.merges.saturating_add(count);
        }
        if self.cells > MAX_CELLS || self.merges > MAX_MERGES || self.text > MAX_TEXT {
            return Err(IoError::TooLarge("ODS expands beyond the import limit (4 million cells, 100000 merges or 256 MiB of text)".into()));
        }
        let last_col = col.checked_add(cols - 1).and_then(|c| c.checked_add(width - 1));
        let last_row = row.checked_add(rows - 1).and_then(|r| r.checked_add(height - 1));
        if last_col.is_none_or(|c| c >= MAX_COLS) || last_row.is_none_or(|r| r >= MAX_ROWS) {
            return Err(IoError::TooLarge("ODS merged cell exceeds worksheet bounds".into()));
        }
        if merged && ((cols > 1 && width > 1) || (rows > 1 && height > 1)) {
            return Err(IoError::Format("repeated ODS cells have overlapping merges".into()));
        }
        // Extras of the cell element: a note, a link, a validation, pictures.
        let mut comment = None;
        let mut link = None;
        let mut validation = None;
        let mut mapped = None;
        let mut frames = Vec::new();
        if let Some(c) = el {
            if let Some(a) = c.child(Ns::Office, "annotation") {
                comment = Some(read_annotation(a));
            }
            if let Some(a) = c.descendants(Ns::Text, "a").first()
                && let Some(href) = a.attr(Ns::XLink, "href")
            {
                link = Some(hyperlink(href, a.attr(Ns::Office, "title")));
            }
            validation = c.attr(Ns::Table, "content-validation-name").map(str::to_string);
            mapped = c
                .attr(Ns::Table, "style-name")
                .filter(|n| self.auto.get(*n).or_else(|| self.named.get(*n)).is_some_and(|d| !d.maps.is_empty()))
                .map(str::to_string);
            for f in c.elems().filter(|k| k.is(Ns::Draw, "frame")) {
                if let Some(fr) = read_frame(f, Some(CellRef::new(row, col))) {
                    frames.push(fr);
                }
            }
        }
        let Some(st) = self.sheet.as_mut() else { return Ok(()) };
        for r in row..row + rows {
            for c in col..col + cols {
                let pos = CellRef::new(r, c);
                if !cell.is_blank() {
                    let mut stored = cell.clone();
                    if let Some(f) = stored.formula.as_mut().filter(|f| f.array.is_some()) {
                        // A matrix formula's range moves with each repeat.
                        let f = Arc::make_mut(f);
                        if let Some(a) = f.array {
                            f.array = Some(RangeRef::new(pos, CellRef::new(pos.row + a.height() - 1, pos.col + a.width() - 1)));
                        }
                    }
                    st.sheet.cells.set(pos, stored);
                }
                if merged {
                    st.sheet.merges.push(RangeRef::new(pos, CellRef::new(r + height - 1, c + width - 1)));
                }
                if let Some(cm) = &comment {
                    st.sheet.comments.insert(pos, cm.clone());
                }
                if let Some(l) = &link {
                    st.sheet.hyperlinks.insert(pos, l.clone());
                }
            }
        }
        if let Some(m) = mapped {
            st.mapped.entry(m).or_default().push(RangeRef::new(CellRef::new(row, col), CellRef::new(row + rows - 1, col + cols - 1)));
        }
        if let Some(v) = validation {
            st.validation_cells.entry(v).or_default().push(RangeRef::new(CellRef::new(row, col), CellRef::new(row + rows - 1, col + cols - 1)));
        }
        st.frames.extend(frames);
        Ok(())
    }

    /// The value, formula and number format of a cell element.
    fn convert(&mut self, c: &Elem, at: CellRef) -> Result<Cell, IoError> {
        let error = c.attr(Ns::CalcExt, "value-type") == Some("error");
        let kind = if error { Some("error") } else { c.attr(Ns::Office, "value-type") };
        let text = || c.paragraphs_text();
        let raw = |k: &str| c.attr(Ns::Office, k);
        let mut fmt: Option<&'static str> = None;
        let formula_text = c.attr(Ns::Table, "formula");
        let value = match kind {
            Some("float" | "percentage" | "currency") => {
                raw("value").and_then(|s| s.trim().parse::<f64>().ok()).filter(|n| n.is_finite()).map(Value::Number)
            }
            Some("boolean") => match raw("boolean-value").map(str::trim) {
                Some("true" | "1") => Some(Value::Bool(true)),
                Some("false" | "0") => Some(Value::Bool(false)),
                _ => None,
            },
            Some("string") => Some(Value::Text(Arc::from(raw("string-value").map(str::to_string).unwrap_or_else(text).as_str()))),
            Some("date") => raw("date-value").and_then(|d| parse_date(d, self.wb.date_system)).map(|(n, f)| {
                fmt = Some(f);
                Value::Number(n)
            }),
            Some("time") => raw("time-value").and_then(parse_duration).map(|n| {
                fmt = Some("[h]:mm:ss");
                Value::Number(n)
            }),
            Some("error") => Some(Value::Error(error_value(text().trim()))),
            Some("void") => Some(Value::Empty),
            None => {
                let t = text();
                Some(if t.is_empty() { Value::Empty } else { Value::Text(Arc::from(t.as_str())) })
            }
            Some(_) => None,
        };
        let value = match value {
            Some(v) => v,
            None => {
                self.report.warn("Some ODS values were unsupported or malformed; their displayed text was kept.");
                let t = text();
                let raw_value = ["date-value", "time-value", "value", "boolean-value"].iter().find_map(|k| raw(k));
                match (t.is_empty(), raw_value) {
                    (false, _) => Value::Text(Arc::from(t.as_str())),
                    (true, Some(r)) => Value::Text(Arc::from(r)),
                    (true, None) => Value::Empty,
                }
            }
        };
        // Older writers cache a formula's error as its text (`Err:508`, `#VALUE!`).
        let value = match value {
            Value::Text(t) if formula_text.is_some() && (t.starts_with("Err:") || CellError::parse(&t).is_some()) => Value::Error(error_value(&t)),
            v => v,
        };
        let mut cell = Cell::value(value);
        if let Some(f) = formula_text {
            match formula::from_odf(f).map(|t| Formula::new(&t)).filter(|f| f.expr.is_some()) {
                Some(mut parsed) => {
                    let mc = c.attr(Ns::Table, "number-matrix-columns-spanned").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                    let mr = c.attr(Ns::Table, "number-matrix-rows-spanned").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                    if mc > 0 && mr > 0 {
                        let end_r = at.row.saturating_add(mr - 1).min(MAX_ROWS - 1);
                        let end_c = at.col.saturating_add(mc - 1).min(MAX_COLS - 1);
                        parsed.array = Some(RangeRef::new(at, CellRef::new(end_r, end_c)));
                    }
                    cell.formula = Some(Arc::new(parsed));
                }
                None => self.report.warn("Some ODS formulas use syntax GridCraft doesn't read; their saved results were kept as values."),
            }
        }
        if let Some(code) = fmt {
            cell.style = self.wb.styles.derive(StyleId::DEFAULT, |s| s.num_fmt = NumFmt::new(code));
        }
        Ok(cell)
    }

    /// LibreOffice shows each paragraph of a cell's text on its own line whether or not the cell
    /// wraps; in the model only wrapped cells break lines, so such cells wrap.
    fn wrap_lines(&mut self, cell: &mut Cell) {
        if let Value::Text(t) = &cell.value
            && t.contains('\n')
            && !self.wb.styles.get(cell.style).align.wrap
        {
            cell.style = self.wb.styles.derive(cell.style, |s| s.align.wrap = true);
        }
    }

    fn finish_sheet(&mut self) {
        let Some(mut st) = self.sheet.take() else { return };
        if st.external {
            return;
        }
        let index = self.wb.sheets.len();
        // Columns: the most common width is the default.
        let default_w = most_common(st.cols.iter().map(|c| (c.size, c.count)));
        if let Some(w) = default_w {
            st.sheet.default_col_width = w;
        }
        let mut entries = 0usize;
        for c in &st.cols {
            let special = c.size != default_w.or(Some(st.sheet.default_col_width)) || c.hidden || c.outline > 0 || c.style.is_some();
            if c.page_break && c.start > 0 {
                st.sheet.print.col_breaks.push(c.start);
            }
            if !special {
                continue;
            }
            for i in c.start..c.start.saturating_add(c.count).min(MAX_COLS) {
                entries += 1;
                if entries > MAX_LINE_ENTRIES {
                    break;
                }
                st.sheet.cols.insert(
                    i,
                    LineInfo {
                        size: c.size.filter(|s| Some(*s) != default_w),
                        hidden: c.hidden,
                        outline: c.outline,
                        collapsed: c.collapsed,
                        style: c.style,
                        custom: true,
                    },
                );
            }
        }
        // Rows: a run that fills the rest of the sheet sets the default height (writers pad the
        // sheet to its end with one); otherwise the most common automatic height is the default.
        let tail = st.rows.last().filter(|r| r.start.saturating_add(r.count) >= MAX_ROWS && r.count > 10_000 && !r.hidden && r.outline == 0);
        let tail_start = tail.map(|r| r.start);
        // Heights marked optimal are recalculated from the content when the file opens (their
        // stored value may be stale): one line of the default font is 1.28 times its size, as
        // LibreOffice lays it out (10 pt → 0.452 cm). Only a manually sized run at the end sets
        // another default.
        let font_h = pt_to_px(f64::from(self.wb.styles.get(StyleId::DEFAULT).font.size) * 1.28);
        let near = |h: f32| (h - font_h).abs() <= font_h * 0.2;
        let default_h = match tail.filter(|r| !r.optimal).and_then(|r| r.size) {
            Some(h) => h,
            None => font_h,
        };
        st.sheet.default_row_height = default_h;
        let default_h = Some(default_h);
        let last = st.last_content_row.unwrap_or(0);
        for r in &st.rows {
            if r.page_break && r.start > 0 {
                st.sheet.print.row_breaks.push(r.start);
            }
            // An optimal height close to one line is the default; a taller one (wrapped or larger
            // text) is kept as the automatic height it was.
            let size = match r.size {
                Some(h) if r.optimal && near(h) => None,
                Some(h) if Some(h) == default_h => None,
                other => other,
            };
            let custom = !r.optimal && size.is_some();
            let special = size.is_some() || r.hidden || r.outline > 0 || r.style.is_some();
            if !special {
                continue;
            }
            // The run that fills the rest of the sheet is the default height already.
            if Some(r.start) == tail_start && (r.start > last || r.style.is_none()) {
                continue;
            }
            for i in r.start..r.start.saturating_add(r.count).min(MAX_ROWS) {
                entries += 1;
                if entries > MAX_LINE_ENTRIES {
                    self.report.warn("Some ODS row and column sizes beyond the import limit were not imported.");
                    break;
                }
                st.sheet.rows.insert(i, LineInfo { size, hidden: r.hidden, outline: r.outline, collapsed: r.collapsed, style: r.style, custom });
            }
        }
        if let Some((a, b)) = st.header_rows {
            st.sheet.print.title_rows = Some((a, b));
        }
        if let Some((a, b)) = st.header_cols {
            st.sheet.print.title_cols = Some((a, b));
        }
        if let Some(pr) = &st.print_ranges {
            let first = pr.split_whitespace().next().unwrap_or_default();
            if let Ok(e) = gridcraft_formula::parse(&formula::reference(first))
                && let gridcraft_formula::Expr::Ref(r) = e
            {
                st.sheet.print.print_area = Some(r.range());
            }
        }
        self.style_map_formats(&mut st);
        // Validations: the cells that name each one.
        let mut vnames: Vec<_> = st.validation_cells.into_iter().collect();
        vnames.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, ranges) in vnames {
            if let Some(def) = self.validations.get(&name) {
                let mut v = def.v.clone();
                v.ranges = merge_ranges(ranges);
                st.sheet.validations.push(v);
            }
        }
        // A merged area shows its first cell's format, as LibreOffice draws it.
        let twin = if st.sheet.merges.is_empty() { StyleId::DEFAULT } else { self.default_twin() };
        for m in st.sheet.merges.clone() {
            let anchor = st.sheet.style_id(m.start);
            // The default style can't be stored over a formatted row or column: its twin can.
            let stored = if anchor == StyleId::DEFAULT { twin } else { anchor };
            for r in m.start.row..=m.end.row {
                for c in m.start.col..=m.end.col {
                    let pos = CellRef::new(r, c);
                    let here = st.sheet.style_id(pos);
                    if pos != m.start && here != anchor && here != stored {
                        st.sheet.set_style(pos, if st.sheet.cell(pos).is_some() && anchor == StyleId::DEFAULT { anchor } else { stored });
                    }
                }
            }
        }
        self.images.extend(st.frames.drain(..).map(|f| (index, f)));
        self.wb.sheets.push(Arc::new(st.sheet));
    }

    fn sheet_index(&self) -> Option<usize> {
        self.sheet.as_ref().map(|_| self.wb.sheets.len())
    }

    fn named_expressions(&mut self, e: &Elem) {
        let scope = self.sheet_index();
        for n in e.elems() {
            let Some(name) = n.attr(Ns::Table, "name") else { continue };
            if n.is(Ns::Table, "named-range") {
                if let Some(addr) = n.attr(Ns::Table, "cell-range-address") {
                    self.names.push((name.to_string(), scope, addr.to_string(), true, n.attr(Ns::Table, "base-cell-address").map(str::to_string)));
                }
            } else if n.is(Ns::Table, "named-expression")
                && let Some(expr) = n.attr(Ns::Table, "expression")
            {
                self.names.push((name.to_string(), scope, expr.to_string(), false, n.attr(Ns::Table, "base-cell-address").map(str::to_string)));
            }
        }
    }

    pub(crate) fn finish_names(&mut self) {
        for (name, scope, text, range, base) in std::mem::take(&mut self.names) {
            let formula = if range {
                // `$Sheet1.$A$1:.$B$2`, possibly several ranges separated by spaces.
                let parts: Vec<String> = text.split_whitespace().map(|p| formula::reference(p.trim_matches(|c| c == '[' || c == ']'))).collect();
                if parts.len() == 1 { parts.into_iter().next().unwrap_or_default() } else { format!("({})", parts.join(",")) }
            } else {
                match formula::from_odf(&text) {
                    Some(t) => t,
                    None => continue,
                }
            };
            // A reference that no longer reads (a deleted sheet) is an error, as it calculates.
            let formula = if gridcraft_formula::parse(&formula).is_ok() { formula } else { "#REF!".to_string() };
            // Relative references are relative to the base cell in ODF, to A1 in the model.
            let formula = match base.as_deref().and_then(range_on_sheet).map(|(_, r)| r.start) {
                Some(b) if b != CellRef::new(0, 0) => rebase_relative(&formula, b),
                _ => formula,
            };
            // Names that clash with the workbook's own rules are kept only when valid.
            if gridcraft_formula::parse_name(&name).is_err() && !name.starts_with('_') {
                self.report.warn("Some ODS named ranges have names GridCraft can't use and were skipped.");
                continue;
            }
            // An unscoped "print range" from older files is the print area.
            if self.wb.names.iter().any(|n| n.name.eq_ignore_ascii_case(&name) && n.scope == scope) {
                continue;
            }
            self.wb.names.push(DefinedName { name, scope, formula, comment: String::new(), hidden: false });
        }
        for db in std::mem::take(&mut self.database_ranges) {
            self.database_range(&db);
        }
    }

    fn database_range(&mut self, db: &Elem) {
        let Some(addr) = db.attr(Ns::Table, "target-range-address") else { return };
        let (sheet, range) = match range_on_sheet(addr) {
            Some(v) => v,
            None => return,
        };
        // A named database range is usable in formulas by its name.
        if let Some(name) = db.attr(Ns::Table, "name")
            && !name.starts_with("__Anonymous")
            && gridcraft_formula::parse_name(name).is_ok()
            && !self.wb.names.iter().any(|n| n.name.eq_ignore_ascii_case(name))
            && self.wb.sheets.iter().any(|s| s.name == sheet)
        {
            let a = gridcraft_formula::Anchor { row: range.start.row, col: range.start.col, row_abs: true, col_abs: true };
            let b = gridcraft_formula::Anchor { row: range.end.row, col: range.end.col, row_abs: true, col_abs: true };
            let r = gridcraft_formula::Reference {
                sheet: gridcraft_formula::SheetSel::Named(sheet.clone()),
                kind: gridcraft_formula::RefKind::Range(a, b),
            };
            self.wb.names.push(DefinedName {
                name: name.to_string(),
                scope: None,
                formula: gridcraft_formula::printer::reference_a1(&r),
                comment: String::new(),
                hidden: false,
            });
        }
        if db.attr(Ns::Table, "display-filter-buttons") != Some("true") {
            return;
        }
        let Some(idx) = self.wb.sheets.iter().position(|s| s.name == sheet) else { return };
        let mut criteria = Vec::new();
        if let Some(filter) = db.child(Ns::Table, "filter") {
            for cond in filter.descendants(Ns::Table, "filter-condition") {
                let Some(field) = cond.attr(Ns::Table, "field-number").and_then(|f| f.parse::<u32>().ok()) else { continue };
                let op = cond.attr(Ns::Table, "operator").unwrap_or("=");
                let value = cond.attr(Ns::Table, "value").unwrap_or_default().to_string();
                let set: Vec<String> =
                    cond.children(Ns::Table, "filter-set-item").filter_map(|i| i.attr(Ns::Table, "value").map(str::to_string)).collect();
                let crit = match op {
                    "=" if !set.is_empty() => gridcraft_model::FilterCriterion::Values { values: set, blanks: false },
                    "=" => gridcraft_model::FilterCriterion::Values { values: vec![value], blanks: false },
                    "empty" => gridcraft_model::FilterCriterion::Values { values: vec![], blanks: true },
                    "top values" | "bottom values" | "top percent" | "bottom percent" => gridcraft_model::FilterCriterion::Top10 {
                        bottom: op.starts_with("bottom"),
                        percent: op.ends_with("percent"),
                        count: value.parse::<f64>().ok().map_or(10, |v| v.clamp(1.0, 500.0) as u32),
                    },
                    o => {
                        let (op, v) = match o {
                            "begins" => ("=", format!("{value}*")),
                            "ends" => ("=", format!("*{value}")),
                            "contains" => ("=", format!("*{value}*")),
                            "!contains" => ("<>", format!("*{value}*")),
                            "!=" => ("<>", value),
                            "<" | ">" | "<=" | ">=" => (o, value),
                            _ => ("=", value),
                        };
                        gridcraft_model::FilterCriterion::Custom { a: (op.to_string(), v), b: None, and: true }
                    }
                };
                criteria.push((field, crit));
            }
        }
        if let Some(sheet) = self.wb.sheets.get_mut(idx) {
            Arc::make_mut(sheet).autofilter = Some(AutoFilter { range, criteria });
        }
    }

    fn content_validations(&mut self, e: &Elem) {
        for cv in e.children(Ns::Table, "content-validation") {
            let Some(name) = cv.attr(Ns::Table, "name") else { continue };
            let mut v = Validation { allow_blank: cv.attr(Ns::Table, "allow-empty-cell") != Some("false"), ..Validation::default() };
            v.in_cell_dropdown = cv.attr(Ns::Table, "display-list") != Some("none");
            if let Some(cond) = cv.attr(Ns::Table, "condition") {
                parse_validation(cond, &mut v);
            }
            if let Some(h) = cv.child(Ns::Table, "help-message") {
                v.input_title = h.attr(Ns::Table, "title").unwrap_or_default().to_string();
                v.input_message = h.paragraphs_text();
                v.show_input = h.attr(Ns::Table, "display") != Some("false");
            }
            if let Some(m) = cv.child(Ns::Table, "error-message") {
                v.error_title = m.attr(Ns::Table, "title").unwrap_or_default().to_string();
                v.error_message = m.paragraphs_text();
                v.show_error = m.attr(Ns::Table, "display") != Some("false");
                v.error_style = match m.attr(Ns::Table, "message-type") {
                    Some("warning") => ErrorStyle::Warning,
                    Some("information") => ErrorStyle::Information,
                    _ => ErrorStyle::Stop,
                };
            } else if cv.child(Ns::Table, "error-macro").is_some() {
                v.show_error = false;
            }
            self.validations.insert(name.to_string(), ValidationDef { v });
        }
    }

    fn conditional_formats(&mut self, e: &Elem) {
        let mut out = Vec::new();
        let mut priority = 1u32;
        for cf in e.children(Ns::CalcExt, "conditional-format") {
            let Some(addr) = cf.attr(Ns::CalcExt, "target-range-address") else { continue };
            let ranges: Vec<RangeRef> = addr.split_whitespace().filter_map(range_on_sheet).map(|(_, r)| r).collect();
            if ranges.is_empty() {
                continue;
            }
            for rule in cf.elems() {
                if let Some(r) = self.cf_rule(rule) {
                    out.push(CondFormat { ranges: ranges.clone(), rule: r, priority, stop_if_true: false });
                    priority += 1;
                }
            }
        }
        if let Some(st) = self.sheet.as_mut() {
            st.sheet.cond_formats.extend(out);
            st.has_calcext_formats = true;
        }
    }

    /// ODF's own conditional styles (`style:map` on a cell style), for files without
    /// LibreOffice's richer `calcext:conditional-formats`.
    fn style_map_formats(&self, st: &mut SheetState) {
        if st.has_calcext_formats {
            return;
        }
        let mut names: Vec<_> = st.mapped.drain().collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        let mut priority = st.sheet.cond_formats.len() as u32 + 1;
        for (name, ranges) in names {
            let Some(def) = self.auto.get(&name).or_else(|| self.named.get(&name)) else { continue };
            let ranges = merge_ranges(ranges);
            for (cond, apply, _) in &def.maps {
                let style = Box::new(self.dxf(apply));
                if let Some(rule) = map_condition(cond, style) {
                    st.sheet.cond_formats.push(CondFormat { ranges: ranges.clone(), rule, priority, stop_if_true: true });
                    priority += 1;
                }
            }
        }
    }

    fn cf_rule(&self, rule: &Elem) -> Option<CfRule> {
        let entry = |e: &Elem| -> CfValueKind {
            let v = e.attr(Ns::CalcExt, "value").unwrap_or("0");
            let n = v.parse::<f64>().unwrap_or(0.0);
            match e.attr(Ns::CalcExt, "type") {
                Some("minimum" | "auto-minimum") => CfValueKind::Min,
                Some("maximum" | "auto-maximum") => CfValueKind::Max,
                Some("percent") => CfValueKind::Percent(n),
                Some("percentile") => CfValueKind::Percentile(n),
                Some("formula") => CfValueKind::Formula(formula::from_odf(v).unwrap_or_else(|| v.to_string())),
                _ => CfValueKind::Number(n),
            }
        };
        match rule.name.as_str() {
            "color-scale" => {
                let stops = rule
                    .children(Ns::CalcExt, "color-scale-entry")
                    .map(|e| (entry(e), e.attr(Ns::CalcExt, "color").and_then(color).unwrap_or(Color::Auto)))
                    .collect();
                Some(CfRule::ColorScale { stops })
            }
            "data-bar" => {
                let entries: Vec<CfValueKind> = rule.children(Ns::CalcExt, "formatting-entry").map(entry).collect();
                Some(CfRule::DataBar {
                    min: entries.first().cloned().unwrap_or(CfValueKind::Min),
                    max: entries.get(1).cloned().unwrap_or(CfValueKind::Max),
                    color: rule.attr(Ns::CalcExt, "positive-color").and_then(color).unwrap_or(Color::Rgb(0x638EC6)),
                    gradient: rule.attr(Ns::CalcExt, "gradient") != Some("false"),
                    show_value: rule.attr(Ns::CalcExt, "show-value") != Some("false"),
                })
            }
            "icon-set" => {
                let set = rule.attr(Ns::CalcExt, "icon-set-type").unwrap_or("3Arrows").to_string();
                let thresholds = rule.children(Ns::CalcExt, "formatting-entry").map(entry).collect();
                Some(CfRule::IconSet { set, thresholds, reverse: false, show_value: rule.attr(Ns::CalcExt, "show-value") != Some("false") })
            }
            "date-is" => {
                let style = Box::new(self.dxf(rule.attr(Ns::CalcExt, "style")?));
                let period = match rule.attr(Ns::CalcExt, "date")? {
                    "today" => "today",
                    "yesterday" => "yesterday",
                    "tomorrow" => "tomorrow",
                    "last-7-days" => "last7Days",
                    "this-week" => "thisWeek",
                    "last-week" => "lastWeek",
                    "next-week" => "nextWeek",
                    "this-month" => "thisMonth",
                    "last-month" => "lastMonth",
                    "next-month" => "nextMonth",
                    _ => return None,
                };
                Some(CfRule::TimePeriod { period: period.into(), style })
            }
            "condition" => {
                let style = Box::new(self.dxf(rule.attr(Ns::CalcExt, "apply-style-name")?));
                let value = rule.attr(Ns::CalcExt, "value")?;
                cf_condition(value, style)
            }
            _ => None,
        }
    }

    fn frames_in(&mut self, shapes: &Elem) {
        let mut out = Vec::new();
        for f in shapes.elems().filter(|e| e.is(Ns::Draw, "frame")) {
            if let Some(fr) = read_frame(f, None) {
                out.push(fr);
            }
        }
        if let Some(st) = self.sheet.as_mut() {
            st.frames.extend(out);
        }
    }

    fn load_pictures(&mut self, package: &mut Package<'_>) {
        let mut next_id = 1u32;
        let mut next_chart = 1u32;
        for (index, f) in std::mem::take(&mut self.images) {
            let path = f.href.trim_start_matches("./").to_string();
            if f.object {
                let part = format!("{}/content.xml", path.trim_end_matches('/'));
                let draft = match package.read_limited(&part, 64 * 1024 * 1024) {
                    Ok(Some(xml)) => dom::parse_tree(&xml).ok().as_ref().and_then(super::chart::read),
                    _ => None,
                };
                let Some(d) = draft else {
                    self.report.warn("Some ODS embedded objects that are not charts (or damaged charts) were not imported.");
                    continue;
                };
                let Some(sheet) = self.wb.sheets.get_mut(index) else { continue };
                let sheet = Arc::make_mut(sheet);
                let anchor = place_anchor(sheet, &f);
                sheet.charts.push(gridcraft_model::Chart {
                    id: next_chart,
                    kind: d.kind,
                    anchor,
                    // No title in the file is no title, not the automatic one.
                    title: Some(d.title.unwrap_or_default()),
                    series: d.series,
                    legend: d.legend,
                    data_labels: d.data_labels,
                    data_table: None,
                    gridlines: d.gridlines,
                    style: 2,
                    x_title: d.x_title,
                    y_title: d.y_title,
                    source: None,
                    by_rows: false,
                    pivot: None,
                });
                next_chart += 1;
                continue;
            }
            let mime = match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
                Some("png") => "image/png",
                Some("jpg" | "jpeg") => "image/jpeg",
                Some("gif") => "image/gif",
                _ => {
                    self.report.warn("Some ODS pictures in formats GridCraft doesn't show were not imported.");
                    continue;
                }
            };
            let Ok(Some(data)) = package.read_limited(&path, 64 * 1024 * 1024) else {
                self.report.warn("Some ODS pictures are linked or missing and were not imported.");
                continue;
            };
            let Some(sheet) = self.wb.sheets.get_mut(index) else { continue };
            let sheet = Arc::make_mut(sheet);
            let anchor = place_anchor(sheet, &f);
            sheet.images.push(Image { id: next_id, anchor, data: Arc::new(data), mime: mime.into(), alt: f.alt.clone() });
            next_id += 1;
        }
    }
}

/// The anchor of a picture: its cell and offset from the sheet origin or its anchor cell.
fn place_anchor(sheet: &Sheet, f: &Frame) -> Anchor {
    let (x, y) = (f.x as f32, f.y as f32);
    let (cell, dx, dy) = match f.cell {
        Some(c) => (c, x - sheet_x(sheet, c.col), y - sheet_y(sheet, c.row)),
        None => {
            let (col, dx) = locate(x, |i| col_width_pt(sheet, i), MAX_COLS);
            let (row, dy) = locate(y, |i| row_height_pt(sheet, i), MAX_ROWS);
            (CellRef::new(row, col), dx, dy)
        }
    };
    // Anchors are measured in pixels at 96 dpi; the file's lengths are points.
    let px = |pt: f32| pt * 96.0 / 72.0;
    Anchor {
        cell,
        dx: px(dx.max(0.0)),
        dy: px(dy.max(0.0)),
        width: px(f.width as f32),
        height: px(f.height as f32),
        mode: gridcraft_model::AnchorMode::MoveAndSize,
    }
}

fn col_width_pt(sheet: &Sheet, i: u32) -> f32 {
    let info = sheet.cols.get(&i);
    if info.is_some_and(|c| c.hidden) {
        return 0.0;
    }
    info.and_then(|c| c.size).unwrap_or(sheet.default_col_width) * 0.75
}
fn row_height_pt(sheet: &Sheet, i: u32) -> f32 {
    let info = sheet.rows.get(&i);
    if info.is_some_and(|c| c.hidden) {
        return 0.0;
    }
    info.and_then(|c| c.size).unwrap_or(sheet.default_row_height) * 0.75
}
fn sheet_x(sheet: &Sheet, col: u32) -> f32 {
    (0..col.min(MAX_COLS)).map(|i| col_width_pt(sheet, i)).sum()
}
fn sheet_y(sheet: &Sheet, row: u32) -> f32 {
    // Rows without an entry all have the default height.
    let custom: f32 = sheet.rows.range(..row).map(|(i, _)| row_height_pt(sheet, *i)).sum();
    let n = sheet.rows.range(..row).count() as f32;
    custom + (row as f32 - n) * sheet.default_row_height * 0.75
}
fn locate(mut pos: f32, size: impl Fn(u32) -> f32, max: u32) -> (u32, f32) {
    let mut i = 0;
    while i + 1 < max {
        let s = size(i);
        if pos < s || i > 100_000 {
            break;
        }
        pos -= s;
        i += 1;
    }
    (i, pos)
}

fn read_frame(f: &Elem, cell: Option<CellRef>) -> Option<Frame> {
    let img = f.child(Ns::Draw, "image");
    let obj = f.child(Ns::Draw, "object");
    let href = img.or(obj).and_then(|i| i.attr(Ns::XLink, "href"))?.to_string();
    let len = |n: &str| f.attr(Ns::Svg, n).and_then(length_pt).unwrap_or(0.0);
    let alt = f.child(Ns::Svg, "title").map(Elem::raw_text).or_else(|| f.child(Ns::Svg, "desc").map(Elem::raw_text)).unwrap_or_default();
    // An embedded object (a chart) comes with a replacement picture that is only its preview.
    let (href, object) = match obj.and_then(|o| o.attr(Ns::XLink, "href")) {
        Some(o) => (o.to_string(), true),
        None => (href, false),
    };
    Some(Frame { href, object, cell, x: len("x"), y: len("y"), width: len("width"), height: len("height"), alt })
}

fn read_annotation(a: &Elem) -> Comment {
    let author = a.child(Ns::Dc, "creator").map(Elem::raw_text).unwrap_or_default();
    let date = a.child(Ns::Dc, "date").map(Elem::raw_text);
    Comment {
        author,
        text: a.paragraphs_text(),
        replies: vec![],
        threaded: false,
        resolved: false,
        visible: a.attr(Ns::Office, "display") == Some("true"),
        dates: date.into_iter().collect(),
        note_box: None,
    }
}

/// An ODF link target: `#Sheet2.A1` is a place in this workbook, anything else a URL or file.
fn hyperlink(href: &str, title: Option<&str>) -> Hyperlink {
    let tooltip = title.map(str::to_string);
    if let Some(local) = href.strip_prefix('#') {
        let target = match super::formula::from_odf(&format!("of:=[{local}]")) {
            Some(t) if !t.starts_with('#') => t.replace('$', ""),
            _ => local.to_string(),
        };
        return Hyperlink { target, tooltip, external: false };
    }
    Hyperlink { target: href.to_string(), tooltip, external: true }
}

/// `$Sheet1.$A$1:.$C$9` → (sheet, range).
fn range_on_sheet(addr: &str) -> Option<(String, RangeRef)> {
    let a1 = formula::reference(addr);
    let e = gridcraft_formula::parse(&a1).ok()?;
    let gridcraft_formula::Expr::Ref(r) = e else { return None };
    let sheet = match &r.sheet {
        gridcraft_formula::SheetSel::Named(n) => n.clone(),
        gridcraft_formula::SheetSel::Span(a, _) => a.clone(),
        gridcraft_formula::SheetSel::Current => String::new(),
    };
    Some((sheet, r.range()))
}

/// `text` with its relative references moved from `base` to A1 (wrapping around the sheet edge,
/// as relative names do).
fn rebase_relative(text: &str, base: CellRef) -> String {
    let Ok(e) = gridcraft_formula::parse(text) else { return text.to_string() };
    let shift = |a: &mut gridcraft_formula::Anchor| {
        if !a.row_abs {
            a.row = ((u64::from(a.row) + u64::from(MAX_ROWS) - u64::from(base.row)) % u64::from(MAX_ROWS)) as u32;
        }
        if !a.col_abs {
            a.col = ((u64::from(a.col) + u64::from(MAX_COLS) - u64::from(base.col)) % u64::from(MAX_COLS)) as u32;
        }
    };
    let e = e.map(&mut |x| match x {
        gridcraft_formula::Expr::Ref(mut r) => {
            match &mut r.kind {
                gridcraft_formula::RefKind::Cell(a) => shift(a),
                gridcraft_formula::RefKind::Range(a, b) => {
                    shift(a);
                    shift(b);
                }
                _ => {}
            }
            gridcraft_formula::Expr::Ref(r)
        }
        other => other,
    });
    gridcraft_formula::print(&e)
}

/// Joins vertically adjacent ranges of the same columns.
fn merge_ranges(mut ranges: Vec<RangeRef>) -> Vec<RangeRef> {
    ranges.sort_by_key(|r| (r.start.col, r.end.col, r.start.row));
    let mut out: Vec<RangeRef> = Vec::new();
    for r in ranges {
        if let Some(last) = out.last_mut()
            && last.start.col == r.start.col
            && last.end.col == r.end.col
            && last.end.row.saturating_add(1) == r.start.row
        {
            last.end.row = r.end.row;
            continue;
        }
        out.push(r);
    }
    out
}

fn most_common(items: impl Iterator<Item = (Option<f32>, u32)>) -> Option<f32> {
    let mut counts: Vec<(f32, u64)> = Vec::new();
    for (size, n) in items {
        let Some(s) = size else { continue };
        match counts.iter_mut().find(|(v, _)| (*v - s).abs() < 0.01) {
            Some((_, c)) => *c += u64::from(n),
            None => counts.push((s, u64::from(n))),
        }
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(v, _)| v)
}

/// Splits `f(a;b)` style arguments at top-level `;` or `,`.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quoted = false;
    let mut bracket = false;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '"' => quoted = !quoted,
            '[' if !quoted => bracket = true,
            ']' if !quoted => bracket = false,
            '(' if !quoted && !bracket => depth += 1,
            ')' if !quoted && !bracket => depth -= 1,
            ';' | ',' if !quoted && !bracket && depth == 0 => {
                out.push(std::mem::take(&mut cur).trim().to_string());
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() || !out.is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn expr_text(s: &str) -> String {
    formula::from_odf(&format!("of:={}", s.trim())).unwrap_or_else(|| s.trim().to_string())
}

/// `name(args)` at the start of `s` → (args, rest).
fn call<'a>(s: &'a str, name: &str) -> Option<(Vec<String>, &'a str)> {
    let rest = s.trim_start().strip_prefix(name)?.trim_start().strip_prefix('(')?;
    let mut depth = 1i32;
    let mut quoted = false;
    for (i, ch) in rest.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some((split_args(rest.get(..i)?), rest.get(i + 1..)?));
                }
            }
            _ => {}
        }
    }
    None
}

fn comparison(s: &str) -> Option<(CfOperator, String)> {
    let s = s.trim();
    for (op, k) in [
        ("<=", CfOperator::LessOrEqual),
        (">=", CfOperator::GreaterOrEqual),
        ("!=", CfOperator::NotEqual),
        ("<>", CfOperator::NotEqual),
        ("==", CfOperator::Equal),
        ("=", CfOperator::Equal),
        ("<", CfOperator::Less),
        (">", CfOperator::Greater),
    ] {
        if let Some(rest) = s.strip_prefix(op) {
            return Some((k, expr_text(rest)));
        }
    }
    None
}

/// `of:cell-content-is-whole-number() and cell-content-is-between(1;10)` and friends (ODF 1.3
/// part 3 §19.618).
fn parse_validation(cond: &str, v: &mut Validation) {
    let body = cond.trim();
    let body = body.split_once(':').filter(|(p, _)| p.chars().all(|c| c.is_ascii_alphanumeric())).map_or(body, |(_, b)| b).trim();
    let (kind, rest) = [
        ("cell-content-is-whole-number()", ValidationKind::Whole),
        ("cell-content-is-decimal-number()", ValidationKind::Decimal),
        ("cell-content-is-date()", ValidationKind::Date),
        ("cell-content-is-time()", ValidationKind::Time),
    ]
    .iter()
    .find_map(|(p, k)| body.strip_prefix(p).map(|r| (*k, r.trim_start().strip_prefix("and").unwrap_or(r).trim())))
    .unwrap_or((ValidationKind::Any, body));
    v.kind = kind;
    if let Some((args, _)) = call(rest, "cell-content-is-in-list") {
        v.kind = ValidationKind::List;
        let lits: Option<Vec<String>> =
            args.iter().map(|a| a.strip_prefix('"').and_then(|x| x.strip_suffix('"')).map(|x| x.replace("\"\"", "\""))).collect();
        v.f1 = match lits {
            Some(items) => format!("\"{}\"", items.join(",")),
            None => args.first().map(|a| format!("={}", expr_text(a))).unwrap_or_default(),
        };
        return;
    }
    if let Some((args, _)) = call(rest, "is-true-formula") {
        v.kind = ValidationKind::Custom;
        v.f1 = args.first().map(|a| expr_text(a)).unwrap_or_default();
        return;
    }
    let (rest, textlen) = match rest.strip_prefix("cell-content-text-length") {
        Some(r) => (r, true),
        None => (rest, false),
    };
    if textlen {
        v.kind = ValidationKind::TextLength;
    }
    let between = |r: &str, name: &str| call(r, name).map(|(a, _)| a);
    let rest_norm = rest.trim_start_matches("()").trim();
    let rest_fn = if textlen { format!("cell-content-text-length{rest}") } else { rest.to_string() };
    for (name, op) in [
        ("cell-content-is-between", CfOperator::Between),
        ("cell-content-is-not-between", CfOperator::NotBetween),
        ("cell-content-text-length-is-between", CfOperator::Between),
        ("cell-content-text-length-is-not-between", CfOperator::NotBetween),
    ] {
        if let Some(args) = between(&rest_fn, name).or_else(|| between(rest, name)) {
            v.op = op;
            v.f1 = args.first().map(|a| expr_text(a)).unwrap_or_default();
            v.f2 = args.get(1).map(|a| expr_text(a));
            if name.contains("text-length") {
                v.kind = ValidationKind::TextLength;
            }
            return;
        }
    }
    let cmp = rest_norm.strip_prefix("cell-content()").unwrap_or(rest_norm);
    if let Some((op, val)) = comparison(cmp) {
        v.op = op;
        v.f1 = val;
    } else if !body.is_empty() && v.kind == ValidationKind::Any {
        v.kind = ValidationKind::Custom;
        v.f1 = expr_text(body);
    }
}

/// A `style:map` condition (`cell-content()>5`, `cell-content-is-between(1;10)`,
/// `is-true-formula(…)`) as a rule.
fn map_condition(cond: &str, style: Box<Style>) -> Option<CfRule> {
    let c = cond.trim();
    let c = c.split_once(':').filter(|(p, _)| p.chars().all(|ch| ch.is_ascii_alphanumeric())).map_or(c, |(_, b)| b).trim();
    if let Some((a, _)) = call(c, "cell-content-is-between") {
        return Some(CfRule::CellIs { op: CfOperator::Between, a: expr_text(a.first()?), b: a.get(1).map(|x| expr_text(x)), style });
    }
    if let Some((a, _)) = call(c, "cell-content-is-not-between") {
        return Some(CfRule::CellIs { op: CfOperator::NotBetween, a: expr_text(a.first()?), b: a.get(1).map(|x| expr_text(x)), style });
    }
    if let Some((a, _)) = call(c, "is-true-formula") {
        return Some(CfRule::Expression { formula: expr_text(a.first()?), style });
    }
    let rest = c.strip_prefix("cell-content()")?;
    let (op, a) = comparison(rest)?;
    Some(CfRule::CellIs { op, a, b: None, style })
}

/// A `calcext:condition` value (`between(1,10)`, `>5`, `begins-with("x")`, `duplicate`…).
fn cf_condition(value: &str, style: Box<Style>) -> Option<CfRule> {
    let v = value.trim();
    let text_arg = |a: &[String]| a.first().map(|s| s.trim_matches('"').replace("\"\"", "\"")).unwrap_or_default();
    if let Some((a, _)) = call(v, "between") {
        return Some(CfRule::CellIs { op: CfOperator::Between, a: expr_text(a.first()?), b: a.get(1).map(|x| expr_text(x)), style });
    }
    if let Some((a, _)) = call(v, "not-between") {
        return Some(CfRule::CellIs { op: CfOperator::NotBetween, a: expr_text(a.first()?), b: a.get(1).map(|x| expr_text(x)), style });
    }
    if let Some((a, _)) = call(v, "formula-is") {
        return Some(CfRule::Expression { formula: expr_text(a.first()?), style });
    }
    for (name, kind) in [("begins-with", 0), ("ends-with", 1), ("contains-text", 2), ("not-contains-text", 3)] {
        if let Some((a, _)) = call(v, name) {
            let text = text_arg(&a);
            return Some(match kind {
                0 => CfRule::BeginsWith { text, style },
                1 => CfRule::EndsWith { text, style },
                2 => CfRule::ContainsText { text, style },
                _ => CfRule::NotContainsText { text, style },
            });
        }
    }
    for (name, bottom, percent) in
        [("top-elements", false, false), ("bottom-elements", true, false), ("top-percent", false, true), ("bottom-percent", true, true)]
    {
        if let Some((a, _)) = call(v, name) {
            let rank = a.first().and_then(|n| n.parse::<f64>().ok()).map_or(10, |n| n.clamp(1.0, 1000.0) as u32);
            return Some(CfRule::Top10 { bottom, percent, rank, style });
        }
    }
    Some(match v {
        "duplicate" => CfRule::Duplicate { style },
        "unique" => CfRule::Unique { style },
        "above-average" | "above-average(" => CfRule::AboveAverage { below: false, equal: false, std_dev: 0, style },
        "below-average" => CfRule::AboveAverage { below: true, equal: false, std_dev: 0, style },
        "above-equal-average" | "above-equal-average(" => CfRule::AboveAverage { below: false, equal: true, std_dev: 0, style },
        "below-equal-average" => CfRule::AboveAverage { below: true, equal: true, std_dev: 0, style },
        "error" | "is-error" => CfRule::Errors { style },
        "no-error" | "is-no-error" => CfRule::NoErrors { style },
        "is-empty" | "empty" => CfRule::Blanks { style },
        "is-not-empty" => CfRule::NoBlanks { style },
        other => {
            let (op, a) = comparison(other)?;
            CfRule::CellIs { op, a, b: None, style }
        }
    })
}

impl Sink for Import {
    fn capture(&mut self, ancestors: &[(Ns, String)], ns: Ns, name: &str) -> bool {
        let in_table = self.sheet.is_some();
        match (ns, name) {
            (Ns::Office, "font-face-decls" | "automatic-styles" | "styles" | "scripts" | "forms" | "event-listeners" | "master-styles") => true,
            (Ns::Table, "table-column" | "table-row" | "named-expressions" | "shapes" | "table-source" | "scenario" | "dde-source") if in_table => {
                true
            }
            (Ns::CalcExt, "conditional-formats") if in_table => true,
            (
                Ns::Table,
                "named-expressions"
                | "database-ranges"
                | "content-validations"
                | "calculation-settings"
                | "label-ranges"
                | "data-pilot-tables"
                | "dde-links"
                | "tracked-changes"
                | "consolidation",
            ) => true,
            (Ns::Text, _) => true,
            (Ns::LoExt | Ns::CalcExt, _) if in_table => true,
            (Ns::Table, "table") => false,
            _ => ancestors.len() > 64,
        }
    }

    fn open(&mut self, el: &Elem) -> Result<(), IoError> {
        if self.depth == 0 && !el.is(Ns::Office, "document-content") {
            return Err(xml_error("expected one office:document-content root"));
        }
        self.depth += 1;
        match (el.ns, el.name.as_str()) {
            (Ns::Office, "spreadsheet") => {
                if self.spreadsheet_seen {
                    return Err(xml_error("multiple spreadsheets in one document"));
                }
                self.spreadsheet_seen = true;
                self.ready_styles();
            }
            (Ns::Table, "table") if self.spreadsheet_seen && self.sheet.is_none() => {
                self.ready_styles();
                self.open_table(el)?;
            }
            (Ns::Table, "table-row-group") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.row_level = st.row_level.saturating_add(1).min(7);
                    st.row_group_hidden.push(el.attr(Ns::Table, "display") == Some("false"));
                }
            }
            (Ns::Table, "table-column-group") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.col_level = st.col_level.saturating_add(1).min(7);
                    st.col_group_hidden.push(el.attr(Ns::Table, "display") == Some("false"));
                }
            }
            (Ns::Table, "table-header-rows") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.in_header_rows = true;
                }
            }
            (Ns::Table, "table-header-columns") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.in_header_cols = true;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn close(&mut self, el: &Elem) -> Result<(), IoError> {
        self.depth = self.depth.saturating_sub(1);
        match (el.ns, el.name.as_str()) {
            (Ns::Table, "table") if self.sheet.as_ref().is_some_and(|s| s.depth == self.depth + 1) => self.finish_sheet(),
            (Ns::Table, "table-row-group") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.row_level = st.row_level.saturating_sub(1);
                    st.row_group_hidden.pop();
                }
            }
            (Ns::Table, "table-column-group") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.col_level = st.col_level.saturating_sub(1);
                    st.col_group_hidden.pop();
                }
            }
            (Ns::Table, "table-header-rows") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.in_header_rows = false;
                }
            }
            (Ns::Table, "table-header-columns") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.in_header_cols = false;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn subtree(&mut self, el: Elem) -> Result<(), IoError> {
        match (el.ns, el.name.as_str()) {
            (Ns::Office, "font-face-decls") => self.read_fonts(&el),
            (Ns::Office, "automatic-styles") => self.read_style_list(&el, false),
            (Ns::Office, "styles") => self.read_style_list(&el, true),
            (Ns::Table, "table-column") => self.column(&el)?,
            (Ns::Table, "table-row") => {
                if self.sheet.is_some() {
                    self.row(&el)?;
                }
            }
            (Ns::Table, "named-expressions") => self.named_expressions(&el),
            (Ns::Table, "database-ranges") => self.database_ranges.extend(el.children(Ns::Table, "database-range").cloned()),
            (Ns::Table, "content-validations") => self.content_validations(&el),
            (Ns::CalcExt, "conditional-formats") => self.conditional_formats(&el),
            (Ns::Table, "shapes") => self.frames_in(&el),
            (Ns::Table, "calculation-settings") => {
                if let Some(iter) = el.child(Ns::Table, "iteration")
                    && iter.attr(Ns::Table, "status") == Some("enable")
                {
                    self.wb.calc.iterative = true;
                    if let Some(n) = iter.attr(Ns::Table, "steps").and_then(|s| s.parse::<u32>().ok()) {
                        self.wb.calc.max_iterations = n.clamp(1, gridcraft_model::CalcSettings::MAX_ITERATIONS);
                    }
                    if let Some(d) = iter.attr(Ns::Table, "maximum-difference").and_then(|s| s.parse::<f64>().ok()) {
                        self.wb.calc.set_max_change(d);
                    }
                }
                // Numbers count days from 1904-01-01: the 1904 date system.
                if let Some(nd) = el.child(Ns::Table, "null-date")
                    && nd.attr(Ns::Table, "date-value") == Some("1904-01-01")
                {
                    self.wb.date_system = DateSystem::D1904;
                }
                if el.attr(Ns::Table, "precision-as-shown") == Some("true") {
                    self.wb.calc.precision_as_displayed = true;
                }
            }
            (Ns::Table, "tracked-changes") => self.report.warn("ODS tracked changes were not imported; the current cell contents were."),
            (Ns::Table, "data-pilot-tables") => self.report.warn("ODS pivot tables were imported as their current values."),
            (Ns::Office, "scripts") if !el.elems().all(|e| e.elems().next().is_none()) => self.report.warn("ODS macros were not imported."),
            _ => {}
        }
        Ok(())
    }
}

/// An error as a cell shows it: Excel's (`#DIV/0!`) or LibreOffice's numbered ones (`Err:532`),
/// which map to the Excel error LibreOffice itself saves them as.
fn error_value(text: &str) -> CellError {
    if let Some(e) = CellError::parse(text) {
        return e;
    }
    match text.strip_prefix("Err:").and_then(|n| n.parse::<u32>().ok()) {
        Some(524) => CellError::Ref,
        Some(525) => CellError::Name,
        Some(532) => CellError::Div0,
        Some(502 | 503 | 523) => CellError::Num,
        Some(_) => CellError::Value,
        None => CellError::NA,
    }
}

/// Support unzoned ISO calendar dates and times. A timezone, invalid date or out-of-range
/// calendar year remains unread; never silently discard an offset or roll an invalid date.
pub(crate) fn parse_date(s: &str, system: DateSystem) -> Option<(f64, &'static str)> {
    let s = s.trim();
    let (date, time) = s.split_once('T').map_or((s, None), |(d, t)| (d, Some(t)));
    if date.len() != 10 || date.get(4..5)? != "-" || date.get(7..8)? != "-" {
        return None;
    }
    let year: i32 = digits(date.get(..4)?)?.parse().ok()?;
    let month: u32 = digits(date.get(5..7)?)?.parse().ok()?;
    let day: u32 = digits(date.get(8..10)?)?.parse().ok()?;
    if !(1899..=9999).contains(&year) || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    // ODF counts days from 1899-12-30, Excel from 1900-01-00 with its 1900 leap-year bug: the
    // same calendar date is kept, so dates show as they did. 1899-12-30 is time-only (serial 0).
    let day_serial = if year == 1899 {
        if month != 12 || day != 30 {
            return None;
        }
        0.0
    } else {
        serial_from_ymd(system, year.into(), month.into(), day.into())?
    };
    let Some(time) = time else { return Some((day_serial, "yyyy-mm-dd")) };
    let mut parts = time.split(':');
    let h = parts.next()?;
    let m = parts.next()?;
    let sec = parts.next()?;
    if parts.next().is_some() || h.len() != 2 || m.len() != 2 || sec.split('.').next()?.len() != 2 {
        return None;
    }
    let h: u32 = digits(h)?.parse().ok()?;
    let m: u32 = digits(m)?.parse().ok()?;
    let sec = decimal(sec)?;
    if h >= 24 || m >= 60 || sec >= 60.0 {
        return None;
    }
    Some((day_serial + (h as f64 * 3600.0 + m as f64 * 60.0 + sec) / 86400.0, "yyyy-mm-dd hh:mm:ss"))
}

fn digits(s: &str) -> Option<&str> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then_some(s)
}
fn decimal(s: &str) -> Option<f64> {
    match s.split_once('.') {
        Some((a, b)) => {
            digits(a)?;
            digits(b)?;
        }
        None => {
            digits(s)?;
        }
    }
    s.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// A bounded ISO day/time duration. Calendar years/months cannot be represented as days.
pub(crate) fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    let (negative, s) = s.strip_prefix('-').map_or((false, s), |s| (true, s));
    let mut rest = s.strip_prefix('P')?;
    let mut total = 0.0;
    let mut any = false;
    if let Some((days, tail)) = rest.split_once('D') {
        total += digits(days)?.parse::<f64>().ok()? * 86400.0;
        rest = tail;
        any = true;
    }
    if let Some(mut time) = rest.strip_prefix('T') {
        let mut time_any = false;
        for (unit, factor) in [('H', 3600.0), ('M', 60.0), ('S', 1.0)] {
            if let Some((n, tail)) = time.split_once(unit) {
                let value = if unit == 'S' { decimal(n)? } else { digits(n)?.parse::<f64>().ok()? };
                total += value * factor;
                time = tail;
                time_any = true;
            }
        }
        if !time.is_empty() || !time_any {
            return None;
        }
        any = true;
    } else if !rest.is_empty() {
        return None;
    }
    let days = total / 86400.0;
    (any && days.is_finite() && days <= 2_958_465.0).then_some(if negative { -days } else { days })
}
