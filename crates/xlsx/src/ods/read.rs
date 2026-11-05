//! OpenDocument Spreadsheet import (ODF 1.3 part 3 §9): sheets, typed values, formulas, cell
//! styles and number formats, merges, column widths and row heights, outlines, notes, links,
//! named ranges, data validation, conditional formatting, autofilters, pictures, frozen panes,
//! sheet settings and document properties.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use gridcraft_core::date::{days_in_month, serial_from_ymd};
use gridcraft_core::{CellError, CellRef, DateSystem, MAX_COLS, MAX_ROWS, RangeRef, Value};
use gridcraft_model::{
    Anchor, AutoFilter, Cell, CfOperator, CfRule, CfValueKind, Color, Comment, CondFormat, DefinedName, ErrorStyle, FilterCriterion, Font, Formula,
    Hyperlink, Image, LineInfo, NumFmt, Sheet, SheetProtection, Style, StyleId, StyleTable, Validation, ValidationKind, Visibility, Workbook,
};

use super::dom::{self, Elem, Ns, Sink};
use super::numfmt::{self, DataStyle};
use super::style::{self, CellStyleDef, Props, color, length_pt, pt_to_px};
use super::{OdsKind, formula, settings};
use crate::package::{MAX_ENTRIES, Package};
use crate::{IoError, ReadReport};

const MAX_CELLS: u64 = 4_000_000;
const MAX_STYLED_BLANKS: u64 = 2_000_000;
const MAX_MERGES: u64 = 100_000;
const MAX_TEXT: u64 = 256 * 1024 * 1024;
/// The most pieces repeated cells are split into at column-style and merge edges, over the
/// workbook: the work a row costs whether or not it stores anything.
const MAX_SEGMENTS: u64 = 32_000_000;
/// The most nodes of page layouts, master pages and database ranges kept until the sheets that
/// use them are read: as many as one whole part may hold.
const MAX_KEPT_NODES: usize = 2_000_000;
/// The most conditions (`style:map`) read from one cell style.
const MAX_STYLE_MAPS: usize = 64;
/// The most cell ranges data validation is read for, over the workbook.
const MAX_VALIDATION_RANGES: usize = 1_000_000;
/// The most pivot tables in a workbook.
const MAX_PIVOTS: usize = 10_000;
const MAX_SHEETS: usize = 1024;
/// The most row and column entries (sizes, visibility, outline, style) in a sheet, and in a
/// workbook.
const MAX_LINE_ENTRIES: usize = 200_000;
const MAX_BOOK_LINE_ENTRIES: usize = 2_000_000;
/// The most conditional-format ranges kept, over all rules (each rule holds its ranges).
const MAX_CF_RANGES: u64 = 1_000_000;
/// The most pictures, charts and shapes in a workbook.
const MAX_FRAMES: usize = 10_000;
const MAX_STYLE_DEPTH: usize = 32;
/// The most a package's small parts (manifest, settings, properties) are read to.
const SMALL_PART: u64 = 16 * 1024 * 1024;
/// LibreOffice's defaults for a document that states none.
const ODS_FONT: &str = "Liberation Sans";
const ODS_FONT_SIZE: f32 = 10.0;

/// The style LibreOffice assumes for a document that states none: styles are read, and the
/// file's Default style is written, as the difference from it.
pub(crate) fn baseline() -> Style {
    Style { font: Font { name: ODS_FONT.into(), size: ODS_FONT_SIZE, minor: false, ..Font::default() }, ..Style::default() }
}

/// Reads only the small, exact mimetype entry (a spreadsheet or a spreadsheet template),
/// including when the ZIP uses compression.
pub(crate) fn has_mimetype(bytes: &[u8]) -> bool {
    use std::io::Read;
    let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else { return false };
    if zip.len() > MAX_ENTRIES {
        return false;
    }
    let Ok(mut file) = zip.by_name("mimetype") else { return false };
    let kinds = [OdsKind::Spreadsheet.mimetype(), OdsKind::Template.mimetype()];
    if !kinds.iter().any(|m| file.size() == m.len() as u64) {
        return false;
    }
    let mut value = Vec::new();
    (&mut file).take(OdsKind::Template.mimetype().len() as u64 + 1).read_to_end(&mut value).is_ok() && kinds.iter().any(|m| value == m.as_bytes())
}

use dom::xml_error;

fn check_manifest(bytes: &[u8]) -> Result<(), IoError> {
    let root = dom::parse_tree(bytes)?;
    if !root.is(Ns::Manifest, "manifest") {
        return Err(xml_error("expected one manifest:manifest root"));
    }
    if !root.descendants(Ns::Manifest, "encryption-data").is_empty() {
        return Err(IoError::Encrypted);
    }
    Ok(())
}

/// Imports an ODS package. Always inspect the returned warnings.
pub fn read_ods(bytes: &[u8]) -> Result<(Workbook, ReadReport), IoError> {
    let mut package = Package::open(bytes)?;
    if !has_mimetype(bytes) {
        return Err(IoError::Format("missing or invalid ODS mimetype".into()));
    }
    if let Some(manifest) = package.read_limited("META-INF/manifest.xml", SMALL_PART)? {
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
    // Settings and properties are small: a damaged or oversized one is skipped, with a warning.
    let settings = package.read_limited("settings.xml", SMALL_PART).ok().flatten().map(|s| dom::parse_tree(&s));
    let meta = package.read_limited("meta.xml", SMALL_PART).ok().flatten().map(|m| dom::parse_tree(&m));
    import.finish(settings, meta, Some(&mut package))
}

/// Whether `bytes` are a flat (single-XML) OpenDocument spreadsheet, `.fods`.
pub fn is_flat_ods(bytes: &[u8]) -> bool {
    let head = bytes.get(..bytes.len().min(16 * 1024)).unwrap_or(bytes);
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with('<') && head.contains(":document") && head.contains("application/vnd.oasis.opendocument.spreadsheet")
}

/// Imports a flat OpenDocument spreadsheet (`.fods`): the parts of a package in one XML
/// document, pictures inline as base64 and charts as inline documents.
pub fn read_fods(bytes: &[u8]) -> Result<(Workbook, ReadReport), IoError> {
    if !is_flat_ods(bytes) {
        return Err(IoError::Format("not a flat OpenDocument spreadsheet".into()));
    }
    let mut import = Import::new();
    import.flat = true;
    dom::parse(bytes, &mut import)?;
    // The settings and properties are inline, under the roots their own parts would have.
    let root = |name: &str, el: Elem| Ok(Elem { ns: Ns::Office, name: name.into(), attrs: vec![], kids: vec![dom::Node::El(el)] });
    let settings = import.flat_settings.take().map(|s| root("document-settings", s));
    let meta = import.flat_meta.take().map(|m| root("document-meta", m));
    import.finish(settings, meta, None)
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

impl LineRun {
    /// What the model holds for each line of the run, showing `size` (`None`: the sheet's
    /// default), or `None` when that is all default. A size the file set by hand is custom; an
    /// optimal one is recalculated.
    fn info(&self, size: Option<f32>) -> Option<LineInfo> {
        let special = size.is_some() || self.hidden || self.outline > 0 || self.style.is_some();
        special.then_some(LineInfo {
            size,
            hidden: self.hidden,
            outline: self.outline,
            collapsed: self.collapsed,
            style: self.style,
            custom: !self.optimal && size.is_some(),
        })
    }
}

struct SheetState {
    sheet: Sheet,
    depth: usize,
    row: u32,
    col: u32,
    cols: Vec<LineRun>,
    rows: Vec<LineRun>,
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
    /// Cells whose style carries `style:map` conditions, by style name.
    mapped: HashMap<String, Vec<RangeRef>>,
    has_calcext_formats: bool,
    /// Merges still open, with their first cell's style: a covered cell shows that style, as
    /// LibreOffice draws a merged area.
    open_merges: OpenRanges<StyleId>,
    /// Matrix formulas still open: the cached results in their other cells are the formula's
    /// to fill, not constants.
    open_matrices: OpenRanges<()>,
    /// The columns' default cell styles as (first, end, style) runs, built once the columns are
    /// read (they precede the rows).
    col_defaults: Option<Vec<(u32, u32, StyleId)>>,
}

/// What a flat document holds inline instead of a part of its package.
enum Inline {
    Picture(Vec<u8>),
    Chart(Box<Elem>),
}

/// A picture as read, before its anchor is placed and its bytes loaded.
#[derive(Default)]
struct Frame {
    href: String,
    /// An embedded object (a chart) rather than a picture.
    object: bool,
    /// A drawing shape rather than a picture.
    shape: Option<super::shape::ShapeDraft>,
    /// A flat document's picture bytes or chart document, held inline.
    inline: Option<Inline>,
    cell: Option<CellRef>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    alt: String,
}

/// A defined name as read, translated once every sheet is known.
struct NameDef {
    name: String,
    scope: Option<usize>,
    /// The ODF expression, or the range address of a named range.
    text: String,
    range: bool,
    /// The cell its relative references are relative to.
    base: Option<String>,
}

/// Text from an attribute, held to what a cell holds ([`dom::MAX_CELL_TEXT`]).
fn capped(s: &str) -> &str {
    s.get(..s.floor_char_boundary(dom::MAX_CELL_TEXT)).unwrap_or(s)
}

/// A cell's own link: the first `text:a` in its paragraphs (not one inside its note or a
/// drawing).
fn cell_link(c: &Elem) -> Option<&Elem> {
    c.elems().filter(|p| p.is(Ns::Text, "p") || p.is(Ns::Text, "h")).find_map(|p| p.descendants(Ns::Text, "a").into_iter().next())
}

/// Ranges (merges, matrix formulas) that may still reach the rows to come, by first column,
/// each with what it carries. They don't overlap, and rows are read top down, so the range
/// starting nearest at or left of a cell is the only one that can hold it, and a range ending
/// above the current row is done for good.
struct OpenRanges<T>(BTreeMap<u32, (RangeRef, T)>);

impl<T: Copy> Default for OpenRanges<T> {
    fn default() -> Self {
        OpenRanges(BTreeMap::new())
    }
}

impl<T: Copy> OpenRanges<T> {
    fn insert(&mut self, r: RangeRef, value: T) {
        self.0.insert(r.start.col, (r, value));
    }

    /// The range holding `at`, and the column where that answer ends (its right edge, or the
    /// next range's left one); ranges that ended above `at` are dropped on the way.
    fn at(&mut self, at: CellRef) -> (Option<(RangeRef, T)>, u32) {
        let next = |open: &BTreeMap<u32, (RangeRef, T)>| open.range(at.col.saturating_add(1)..).next().map_or(MAX_COLS, |(k, _)| *k);
        loop {
            let Some((&key, &(r, value))) = self.0.range(..=at.col).next_back() else { return (None, next(&self.0)) };
            if r.end.row < at.row {
                self.0.remove(&key);
                continue;
            }
            return if r.contains(at) { (Some((r, value)), r.end.col.saturating_add(1)) } else { (None, next(&self.0)) };
        }
    }
}

/// A run of cells as a row holds it before it is placed.
struct RowCell<'a> {
    col: u32,
    count: u32,
    /// Columns and rows a merge's first cell spans (1 and 1 otherwise).
    width: u32,
    height: u32,
    cell: Cell,
    /// The style the cell shows when it isn't stored: its row's or column's.
    fallback: StyleId,
    /// What each of its positions holds besides the cell: a note and a link.
    extras: &'a Extras,
    /// Whether one copy of the cell's text was counted already, as it was decoded.
    prepaid: bool,
}

/// What hangs off a cell element at each position it repeats to.
#[derive(Default)]
struct Extras {
    comment: Option<Comment>,
    link: Option<Hyperlink>,
}

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
    draw: super::shape::DrawStyles,
    validations: HashMap<String, Validation>,
    names: Vec<NameDef>,
    database_ranges: Vec<Elem>,
    sheet: Option<SheetState>,
    styles_ready: bool,
    pub spreadsheet_seen: bool,
    /// A flat document (`.fods`): every part in one XML tree.
    flat: bool,
    flat_meta: Option<Elem>,
    flat_settings: Option<Elem>,
    depth: usize,
    /// The depth of a table being skipped: a cached copy of another document's sheet (the
    /// source of an external reference), which is not a sheet of this workbook.
    skip_depth: Option<usize>,
    cells: u64,
    blanks: u64,
    merges: u64,
    text: u64,
    segments: u64,
    kept_nodes: usize,
    pivots: usize,
    validation_ranges: usize,
    images: Vec<(usize, Frame)>,
    cf_ranges: u64,
    /// Pictures, charts and shapes read, and row/column entries stored, over the workbook.
    frames: usize,
    line_entries: usize,
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
            draw: super::shape::DrawStyles::default(),
            validations: HashMap::new(),
            names: Vec::new(),
            database_ranges: Vec::new(),
            sheet: None,
            styles_ready: false,
            spreadsheet_seen: false,
            skip_depth: None,
            flat: false,
            flat_meta: None,
            flat_settings: None,
            depth: 0,
            cells: 0,
            blanks: 0,
            merges: 0,
            text: 0,
            segments: 0,
            kept_nodes: 0,
            pivots: 0,
            validation_ranges: 0,
            images: Vec::new(),
            cf_ranges: 0,
            frames: 0,
            line_entries: 0,
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
        self.draw.add(list);
        for e in list.elems() {
            if e.is(Ns::Style, "default-style") && e.attr(Ns::Style, "family") == Some("table-cell") {
                self.default_props = style::read_props(e, &self.fonts);
                if let Some(tp) = e.child(Ns::Style, "text-properties") {
                    self.wb.text_defaults = style::read_text_defaults(tp, &self.fonts);
                }
                continue;
            }
            if e.is(Ns::Style, "page-layout") {
                if let Some(n) = e.attr(Ns::Style, "name")
                    && self.keep(e)
                {
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
                    // Each sheet translates a style's conditions anew: a style holds only so many.
                    if e.children(Ns::Style, "map").nth(MAX_STYLE_MAPS).is_some() {
                        self.report.warn("Some ODS conditional styles with more conditions than GridCraft reads lost the rest.");
                    }
                    let maps = e
                        .children(Ns::Style, "map")
                        .take(MAX_STYLE_MAPS)
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
            self.read_master_pages(m);
        }
    }

    /// An automatic style, or the named style of that name when none is automatic.
    fn style_def(&self, name: &str) -> Option<&CellStyleDef> {
        self.auto.get(name).or_else(|| self.named.get(name))
    }

    /// The properties `def` states and those its ancestors (named styles) state, nearest first,
    /// up to the document's Default.
    fn ancestry<'s>(&'s self, def: &'s CellStyleDef) -> Vec<&'s Props> {
        let mut chain = vec![&def.props];
        let mut parent = def.parent.as_deref();
        while let Some(n) = parent.take() {
            if n == "Default" || chain.len() >= MAX_STYLE_DEPTH {
                break;
            }
            let Some(d) = self.named.get(n) else { break };
            chain.push(&d.props);
            parent = d.parent.as_deref();
        }
        chain
    }

    /// The document's Default cell style: the default style (`style:default-style`) and what
    /// the named Default style sets.
    fn default_props(&self) -> Props {
        match self.named.get("Default") {
            Some(d) => style::merge(&self.default_props, &d.props),
            None => self.default_props.clone(),
        }
    }

    /// Everything `def` shows. Every cell style is under the document's Default, whether or not
    /// its chain names it, as LibreOffice reads them.
    fn resolve(&self, def: &CellStyleDef) -> Props {
        self.ancestry(def).into_iter().rev().fold(self.default_props(), |p, c| style::merge(&p, c))
    }

    /// The style `p` describes on top of `base`, its data style's format included.
    fn props_style(&self, p: &Props, base: &Style) -> Style {
        let code = p.data_style.as_deref().and_then(|d| numfmt::code(&self.data_styles, d));
        style::to_style(p, base, code)
    }

    fn base_style(&self) -> Style {
        self.props_style(&self.default_props(), &baseline())
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
        let props = match self.style_def(name) {
            Some(def) => self.resolve(def),
            None => self.default_props(),
        };
        // The resolved properties are complete, so they apply to LibreOffice's baseline.
        let id = self.wb.styles.intern(self.props_style(&props, &baseline()));
        self.ids.insert(name.to_string(), id);
        id
    }

    /// A style that looks exactly like the default one but is stored apart from it: the model
    /// drops cells in the default style, so a cell that overrides its row's or column's format with
    /// the default needs it. Only the font family class differs (none and 2 are both swiss).
    fn default_twin(&mut self) -> StyleId {
        self.wb.styles.derive(StyleId::DEFAULT, |s| s.font.family = if s.font.family.is_none() { Some(2) } else { None })
    }

    /// The differential style of a conditional format's named style: only what it and its
    /// ancestors below Default state.
    fn dxf(&self, name: &str) -> Style {
        let Some(def) = self.named.get(name).or_else(|| self.auto.get(name)).filter(|_| name != "Default") else { return Style::default() };
        let p = self.ancestry(def).into_iter().rev().fold(Props::default(), |p, c| style::merge(&p, c));
        self.props_style(&p, &Style::default())
    }

    fn open_table(&mut self, e: &Elem) -> Result<(), IoError> {
        if self.wb.sheets.len() >= MAX_SHEETS {
            return Err(IoError::TooLarge("ODS contains more than 1024 sheets".into()));
        }
        // The sheet keeps the file's name while the file is read, as everything in it refers
        // to it by that name; a name GridCraft can't hold is changed once the file is read.
        let name = e.attr(Ns::Table, "name").unwrap_or_default().trim_matches('\'').to_string();
        let mut sheet = Sheet::new(name);
        let ts = e.attr(Ns::Table, "style-name").and_then(|n| self.table_styles.get(n));
        if let Some(ts) = ts {
            if ts.hidden {
                sheet.visibility = Visibility::Hidden;
            }
            sheet.tab_color = ts.tab_color;
            sheet.right_to_left = ts.rtl;
        }
        let master = ts.and_then(|t| t.master_page.as_deref()).unwrap_or("Default");
        super::page::apply(&mut sheet, master, &self.master_pages, &self.page_layouts);
        if e.attr(Ns::Table, "protected") == Some("true") {
            let password = self.protection_key(e);
            sheet.protection = Some(SheetProtection { password, select_locked: true, select_unlocked: true, ..SheetProtection::default() });
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
            mapped: HashMap::new(),
            has_calcext_formats: false,
            open_merges: OpenRanges::default(),
            open_matrices: OpenRanges::default(),
            col_defaults: None,
        });
        Ok(())
    }

    /// The password of a protected sheet or workbook structure (`table:protection-key`): kept
    /// when it is a SHA-256 key, the algorithm LibreOffice writes; an older SHA-1 key can't be
    /// checked, so the protection is kept without its password.
    fn protection_key(&mut self, e: &Elem) -> Option<gridcraft_model::PasswordHash> {
        let key = e.attr(Ns::Table, "protection-key").filter(|k| !k.trim().is_empty())?;
        let algorithm = e.attr(Ns::Table, "protection-key-digest-algorithm").unwrap_or("http://www.w3.org/2000/09/xmldsig#sha1");
        if algorithm.ends_with("#sha256") && e.attr(Ns::LoExt, "protection-key-digest-algorithm-2").is_none() {
            return Some(gridcraft_model::PasswordHash::Sha256Digest(key.trim().to_string()));
        }
        self.report.warn("Some ODS protection passwords use a digest GridCraft can't check; the protection was kept without its password.");
        None
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
            outline: st.col_group_hidden.len().min(7) as u8,
            collapsed: collapsed && hidden,
            style: default_style.filter(|s| *s != StyleId::DEFAULT),
            page_break: style.page_break,
        });
        Ok(())
    }

    /// The default cell style a row or column states (`table:default-cell-style-name`), when it
    /// is not the default style.
    fn default_cell_style(&mut self, e: &Elem) -> Option<StyleId> {
        let id = self.style_id(e.attr(Ns::Table, "default-cell-style-name")?);
        (id != StyleId::DEFAULT).then_some(id)
    }

    /// The style a cell is stored with: its own (or, when it is covered by a merge, the merge's
    /// first cell's), else what it falls back to, with the format its value implies (a date or
    /// time) and line wrapping for typed text of several lines (LibreOffice shows each paragraph
    /// on its own line, wrapped or not; the model breaks lines only in wrapped cells). The model
    /// drops a stored cell in the default style, so a cell that sets the default back over a
    /// formatted row or column gets the default's look-alike.
    fn cell_style(&mut self, own: Option<StyleId>, fallback: StyleId, implied: Option<&str>, cell: &Cell) -> StyleId {
        let mut style = own.unwrap_or(fallback);
        if let Some(code) = implied
            && self.wb.styles.get(style).num_fmt.as_str().eq_ignore_ascii_case("General")
        {
            style = self.wb.styles.derive(style, |st| st.num_fmt = NumFmt::new(code));
        }
        if cell.formula.is_none() && matches!(&cell.value, Value::Text(t) if t.contains('\n')) && !self.wb.styles.get(style).align.wrap {
            style = self.wb.styles.derive(style, |st| st.align.wrap = true);
        }
        if style == StyleId::DEFAULT && fallback != StyleId::DEFAULT {
            style = self.default_twin();
        }
        style
    }

    fn row(&mut self, e: &Elem) -> Result<(), IoError> {
        if self.sheet.is_none() {
            return Ok(());
        }
        let repeat = attr_u32(e, Ns::Table, "number-rows-repeated", 1, u32::MAX)?;
        let rs = e.attr(Ns::Table, "style-name").and_then(|n| self.row_styles.get(n)).cloned().unwrap_or_default();
        let hidden = matches!(e.attr(Ns::Table, "visibility"), Some("collapse" | "filter"));
        let row_default = self.default_cell_style(e);
        let Some(st) = self.sheet.as_mut() else { return Ok(()) };
        let start_row = st.row;
        let end = start_row.checked_add(repeat).filter(|n| *n <= MAX_ROWS);
        if start_row >= MAX_ROWS {
            // Past the sheet's last row nothing is read; only content is worth a warning.
            if e.elems()
                .any(|c| c.attr(Ns::Office, "value-type").is_some() || c.attr(Ns::Table, "formula").is_some() || c.child(Ns::Text, "p").is_some())
            {
                self.report.warn("Some ODS rows beyond row 1,048,576 were not imported.");
            }
            return Ok(());
        }
        let mut open_merges = std::mem::take(&mut st.open_merges);
        if st.col_defaults.is_none() {
            // Adjacent runs of one style are one run.
            let mut runs: Vec<(u32, u32, StyleId)> = Vec::new();
            for (a, b, s) in st.cols.iter().filter_map(|c| c.style.map(|s| (c.start, c.start + c.count, s))) {
                match runs.last_mut() {
                    Some((_, end, style)) if *end == a && *style == s => *end = b,
                    _ => runs.push((a, b, s)),
                }
            }
            st.col_defaults = Some(runs);
        }
        let col_defaults = st.col_defaults.take().unwrap_or_default();
        let rows = end.unwrap_or(MAX_ROWS) - start_row;
        let last_row = start_row + rows.max(1) - 1;
        let mut col = 0u32;
        let mut row_has_content = false;
        let holds_content =
            |c: &Elem| c.attr(Ns::Office, "value-type").is_some() || c.attr(Ns::Table, "formula").is_some() || c.child(Ns::Text, "p").is_some();
        for (i, c) in e.elems().enumerate() {
            let covered = c.is(Ns::Table, "covered-table-cell");
            if !(c.is(Ns::Table, "table-cell") || covered) {
                continue;
            }
            let crepeat = attr_u32(c, Ns::Table, "number-columns-repeated", 1, u32::MAX)?;
            if col >= MAX_COLS {
                // This cell and the rest of the row lie past the sheet's last column.
                if e.elems().skip(i).any(holds_content) {
                    self.report.warn("Some ODS columns beyond column XFD were not imported.");
                }
                break;
            }
            let width = if covered { 1 } else { attr_u32(c, Ns::Table, "number-columns-spanned", 1, MAX_COLS)? };
            let height = if covered { 1 } else { attr_u32(c, Ns::Table, "number-rows-spanned", 1, MAX_ROWS)? };
            let (mut value_cell, implied, mut prepaid) = self.convert(c, CellRef::new(start_row, col))?;
            self.matrix_cell(
                &mut value_cell,
                RangeRef::new(CellRef::new(start_row, col), CellRef::new(last_row, col.saturating_add(crepeat.saturating_sub(1)).min(MAX_COLS - 1))),
            );
            let own_style = c.attr(Ns::Table, "style-name").map(|n| self.style_id(n));
            let cend = match col.checked_add(crepeat).filter(|n| *n <= MAX_COLS) {
                Some(n) => n,
                None => {
                    if value_cell.has_content() {
                        self.report.warn("Some ODS columns beyond column XFD were not imported.");
                    }
                    MAX_COLS
                }
            };
            let extras = self.element_extras(c, RangeRef::new(CellRef::new(start_row, col), CellRef::new(last_row, cend - 1)));
            // An empty cell with no style, span, note or link of its own shows what it falls
            // back to wherever it lies: nothing to store, so nothing to split.
            let plain = !covered
                && own_style.is_none()
                && implied.is_none()
                && !value_cell.has_content()
                && width == 1
                && height == 1
                && extras.comment.is_none()
                && extras.link.is_none();
            // A repeat spanning columns with different default styles is split at their edges.
            let mut seg = if plain { cend } else { col };
            while seg < cend {
                self.segments += 1;
                if self.segments > MAX_SEGMENTS {
                    return Err(IoError::TooLarge("ODS cells are split into more pieces than the import limit".into()));
                }
                // The column runs are in order: the one holding `seg` is the last starting at or
                // before it, and the next one starts where `seg`'s run of no default ends.
                let at = col_defaults.partition_point(|(a, _, _)| *a <= seg);
                let col_default = at.checked_sub(1).and_then(|i| col_defaults.get(i)).filter(|(_, b, _)| seg < *b);
                let mut seg_end = match col_default {
                    Some((_, b, _)) => (*b).min(cend),
                    None => col_defaults.get(at).map_or(cend, |(a, _, _)| (*a).min(cend)),
                };
                // A covered run is split at the edges of the merges it lies under.
                let mut own = own_style;
                if covered {
                    let (merge, edge) = open_merges.at(CellRef::new(start_row, seg));
                    let anchor = merge.map(|(_, style)| style);
                    seg_end = seg_end.min(edge.max(seg + 1));
                    own = anchor.or(own_style);
                }
                // What the model shows for a cell it doesn't hold: the row's style, else the column's.
                let fallback = row_default.or(col_default.map(|(_, _, s)| *s)).unwrap_or(StyleId::DEFAULT);
                let mut cell = value_cell.clone();
                cell.style = self.cell_style(own, fallback, implied, &cell);
                if width > 1 || height > 1 {
                    let to = CellRef::new(
                        last_row.saturating_add(height - 1).min(MAX_ROWS - 1),
                        (seg_end - 1).saturating_add(width - 1).min(MAX_COLS - 1),
                    );
                    open_merges.insert(RangeRef::new(CellRef::new(start_row, seg), to), cell.style);
                }
                if cell.has_content() || cell.style != fallback || width > 1 || height > 1 || extras.comment.is_some() || extras.link.is_some() {
                    row_has_content |= cell.has_content();
                    // Placed at once, so the import limits apply as the row is read.
                    self.place(start_row, rows, RowCell { col: seg, count: seg_end - seg, width, height, cell, fallback, extras: &extras, prepaid })?;
                    prepaid = false;
                }
                seg = seg_end;
            }
            col = cend;
        }
        if row_has_content && end.is_none() {
            self.report.warn("Some ODS rows beyond row 1,048,576 were not imported.");
        }
        let end = start_row + rows;
        let Some(st) = self.sheet.as_mut() else { return Ok(()) };
        st.open_merges = open_merges;
        st.col_defaults = Some(col_defaults);
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
                outline: st.row_group_hidden.len().min(7) as u8,
                collapsed: collapsed && hidden,
                style: row_default,
                page_break: rs.page_break,
            });
        }
        st.row = end;
        Ok(())
    }

    /// A cell element over `area`: a matrix formula's range is noted, and a cached result inside
    /// one is dropped (the formula fills it again), as the XLSX reader does.
    fn matrix_cell(&mut self, cell: &mut Cell, area: RangeRef) {
        let Some(st) = self.sheet.as_mut() else { return };
        if let Some(range) = cell.formula.as_ref().and_then(|f| f.array) {
            if range.start != range.end {
                st.open_matrices.insert(range, ());
            }
            return;
        }
        if cell.formula.is_some() || matches!(cell.value, Value::Empty) {
            return;
        }
        if let (Some((m, ())), _) = st.open_matrices.at(area.start)
            && m.contains(area.end)
        {
            cell.value = Value::Empty;
        }
    }

    /// Whether an empty cell's own format is worth a stored cell for each of `count` copies:
    /// always for a few, and for long runs (what writers emit for whole-column formatting) only
    /// when it looks different from what the cell falls back to — its fill or borders.
    fn blank_worth_storing(&mut self, style: StyleId, fallback: StyleId, count: u64) -> bool {
        if count <= 1000 {
            return true;
        }
        let (a, b) = (self.wb.styles.get(style), self.wb.styles.get(fallback));
        let visible = a.fill != b.fill || a.borders != b.borders;
        if !visible {
            self.report.warn("Some ODS formatting of long runs of empty cells (other than fill and borders) was not imported.");
        }
        visible
    }

    /// Stores a (repeated) cell and what hangs off it: merges, notes, links, validation, pictures.
    /// What a cell element covering `range` brings besides its value: its validation, its
    /// style's conditions and its pictures (each once, for the whole range), and the note and
    /// link each of its positions holds.
    fn element_extras(&mut self, el: &Elem, range: RangeRef) -> Extras {
        let validation = el.attr(Ns::Table, "content-validation-name").map(str::to_string);
        let mapped = el.attr(Ns::Table, "style-name").filter(|n| self.style_def(n).is_some_and(|d| !d.maps.is_empty())).map(str::to_string);
        self.read_drawings(el, Some(range.start));
        if let Some(st) = self.sheet.as_mut() {
            if let Some(m) = mapped {
                st.mapped.entry(m).or_default().push(range);
            }
            if let Some(v) = validation {
                if self.validation_ranges < MAX_VALIDATION_RANGES {
                    self.validation_ranges += 1;
                    st.validation_cells.entry(v).or_default().push(range);
                } else {
                    self.report.warn("Some ODS data validation beyond the import limit was not imported.");
                }
            }
        }
        Extras {
            // Each position's copy of the note counts as it is stored (see `place`).
            comment: el.child(Ns::Office, "annotation").map(read_annotation),
            link: cell_link(el).and_then(|a| Some(hyperlink(a.attr(Ns::XLink, "href")?, a.attr(Ns::Office, "title")))),
        }
    }

    /// Stores a (repeated) cell and what each of its positions holds: merges, notes, links.
    fn place(&mut self, row: u32, rows: u32, k: RowCell<'_>) -> Result<(), IoError> {
        let RowCell { col, count: cols, mut width, mut height, cell, fallback, extras, prepaid } = k;
        if (cols > 1 && width > 1) || (rows > 1 && height > 1) {
            return Err(IoError::Format("repeated ODS cells have overlapping merges".into()));
        }
        // A merge reaching past the sheet's edge ends at it (the cells themselves never pass it).
        let room_w = MAX_COLS.saturating_sub((col + cols).saturating_sub(1));
        let room_h = MAX_ROWS.saturating_sub((row + rows).saturating_sub(1));
        if width > room_w || height > room_h {
            self.report.warn("Some ODS merged cells reaching past the sheet's edge were cut at it.");
            width = width.min(room_w).max(1);
            height = height.min(room_h).max(1);
        }
        let count = u64::from(rows) * u64::from(cols);
        let merged = width > 1 || height > 1;
        // A cell is stored for its content, or as a formatted empty cell within its own budget;
        // one that looks like what it falls back to needs no entry.
        let mut store = cell.has_content();
        if store {
            self.cells = self.cells.saturating_add(count);
            if let Value::Text(t) = &cell.value {
                let copies = count.saturating_sub(u64::from(prepaid));
                self.text = self.text.saturating_add(copies.saturating_mul(t.len() as u64));
            }
        } else if cell.style != fallback && self.blank_worth_storing(cell.style, fallback, count) {
            self.blanks = self.blanks.saturating_add(count);
            store = self.blanks <= MAX_STYLED_BLANKS;
            if !store {
                self.report.warn("Some ODS formatting of empty cells beyond the import limit was not imported.");
            }
        }
        if merged {
            self.merges = self.merges.saturating_add(count);
        }
        // Each copy of a note, link or matrix formula is stored: they count as cells, and their
        // text as text.
        let Extras { comment, link } = extras;
        if comment.is_some() || link.is_some() {
            self.cells = self.cells.saturating_add(count);
        }
        let copied = comment.as_ref().map_or(0, |c| c.text.len() + c.author.len())
            + link.as_ref().map_or(0, |l| l.target.len() + l.tooltip.as_ref().map_or(0, String::len))
            + cell.formula.as_ref().filter(|f| store && f.array.is_some()).map_or(0, |f| f.text.len());
        self.text = self.text.saturating_add(count.saturating_mul(copied as u64));
        if self.cells > MAX_CELLS || self.merges > MAX_MERGES || self.text > MAX_TEXT {
            return Err(IoError::TooLarge("ODS expands beyond the import limit (4 million cells, 100000 merges or 256 MiB of text)".into()));
        }
        let store = store && !cell.is_blank();
        let Some(st) = self.sheet.as_mut() else { return Ok(()) };
        if !(store || merged || comment.is_some() || link.is_some()) {
            return Ok(());
        }
        for r in row..row + rows {
            for c in col..col + cols {
                let pos = CellRef::new(r, c);
                if store {
                    let mut stored = cell.clone();
                    if let Some(f) = stored.formula.as_mut().filter(|f| f.array.is_some()) {
                        // A matrix formula's range moves with each repeat, within the sheet.
                        let f = Arc::make_mut(f);
                        if let Some(a) = f.array {
                            let end = CellRef::new((pos.row + a.height() - 1).min(MAX_ROWS - 1), (pos.col + a.width() - 1).min(MAX_COLS - 1));
                            f.array = Some(RangeRef::new(pos, end));
                        }
                    }
                    st.sheet.cells.set(pos, stored);
                }
                if merged {
                    st.sheet.merges.push(RangeRef::new(pos, CellRef::new(r + height - 1, c + width - 1)));
                }
                if let Some(cm) = comment {
                    st.sheet.comments.insert(pos, cm.clone());
                }
                if let Some(l) = link {
                    st.sheet.hyperlinks.insert(pos, l.clone());
                }
            }
        }
        Ok(())
    }

    /// The value and formula of a cell element, with the number format its value type implies
    /// (a date or a time) for a cell whose style doesn't set one.
    /// Also whether the cell's text is the paragraph text counted as it was decoded.
    fn convert(&mut self, c: &Elem, at: CellRef) -> Result<(Cell, Option<&'static str>, bool), IoError> {
        let error = c.attr(Ns::CalcExt, "value-type") == Some("error");
        let kind = if error { Some("error") } else { c.attr(Ns::Office, "value-type") };
        // Paragraph text is counted as it is decoded (a `text:s` run expands), whatever then
        // becomes of it; the copies a repeat stores count again when they are stored.
        let decoded = std::cell::Cell::new(0usize);
        let text = || {
            let t = c.paragraphs_text();
            decoded.set(decoded.get().saturating_add(t.len()));
            t
        };
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
            Some("string") => Some(Value::Text(Arc::from(raw("string-value").map(|v| capped(v).to_string()).unwrap_or_else(text)))),
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
                    (true, Some(r)) => Value::Text(Arc::from(capped(r))),
                    (true, None) => Value::Empty,
                }
            }
        };
        // Older writers cache a formula's error as its text (`Err:508`, `#VALUE!`).
        let value = match value {
            // A file that states the result's type (`calcext:value-type`) says when it is text.
            Value::Text(t) if formula_text.is_some() && c.attr(Ns::CalcExt, "value-type").is_none() && is_error_text(&t) => {
                Value::Error(error_value(&t))
            }
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
        self.charge_text(decoded.get())?;
        let counted = matches!(&cell.value, Value::Text(t) if decoded.get() >= t.len() && decoded.get() > 0);
        Ok((cell, fmt, counted))
    }

    fn finish_sheet(&mut self) {
        let Some(mut st) = self.sheet.take() else { return };
        let index = self.wb.sheets.len();
        // Columns: the most common width is the default.
        let default_w = most_common(st.cols.iter().map(|c| (c.size, c.count)));
        if let Some(w) = default_w {
            st.sheet.default_col_width = w;
        }
        // The sheet's budget of row and column entries, within what the workbook has left.
        let mut budget = MAX_LINE_ENTRIES.min(MAX_BOOK_LINE_ENTRIES.saturating_sub(self.line_entries));
        let start_budget = budget;
        let mut capped = false;
        for c in &st.cols {
            if c.page_break && c.start > 0 {
                st.sheet.print.col_breaks.push(c.start);
            }
            let size = c.size.filter(|w| !default_w.is_some_and(|d| same_size(*w, d)));
            if let Some(info) = c.info(size) {
                capped |= !store_run(&mut st.sheet.cols, c, MAX_COLS, info, &mut budget);
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
        let default_h = tail.filter(|r| !r.optimal).and_then(|r| r.size).unwrap_or(font_h);
        st.sheet.default_row_height = default_h;
        let last = st.last_content_row;
        for r in &st.rows {
            if r.page_break && r.start > 0 {
                st.sheet.print.row_breaks.push(r.start);
            }
            // The run that fills the rest of the sheet is the default height already. Its cell
            // style past the content can't be kept for a million rows: it is reported.
            if Some(r.start) == tail_start && (last.is_none_or(|l| r.start > l) || r.style.is_none()) {
                if r.style.is_some() {
                    self.report.warn("Some ODS formatting of whole empty rows to the end of a sheet was not imported.");
                }
                continue;
            }
            // An optimal height close to one line is one line of the default font (the sheet's
            // default, unless a manual tail set another); a taller one (wrapped or larger text)
            // is kept as the automatic height it was.
            let size = match r.size {
                Some(h) if r.optimal && near(h) => Some(font_h),
                other => other,
            }
            .filter(|h| !same_size(*h, default_h));
            if let Some(info) = r.info(size) {
                capped |= !store_run(&mut st.sheet.rows, r, MAX_ROWS, info, &mut budget);
            }
        }
        self.line_entries += start_budget - budget;
        if capped {
            self.report.warn("Some ODS row and column sizes beyond the import limit were not imported.");
        }
        if let Some((a, b)) = st.header_rows {
            st.sheet.print.title_rows = Some((a, b));
        }
        if let Some((a, b)) = st.header_cols {
            st.sheet.print.title_cols = Some((a, b));
        }
        if let Some(pr) = &st.print_ranges {
            let first = formula::split_addresses(pr).first().copied().unwrap_or_default();
            if let Some(r) = formula::parse_odf_ref(first) {
                st.sheet.print.print_area = Some(r.range());
            }
        }
        self.style_map_formats(&mut st);
        // Validations: the cells that name each one.
        let mut vnames: Vec<_> = st.validation_cells.into_iter().collect();
        vnames.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, ranges) in vnames {
            // Each sheet holds its own copy: its text counts as the sheet's.
            let Some(bytes) = self
                .validations
                .get(&name)
                .map(|d| d.f1.len() + d.f2.as_ref().map_or(0, String::len) + d.input_message.len() + d.error_message.len())
            else {
                continue;
            };
            if !self.take_text(bytes) {
                break;
            }
            if let Some(def) = self.validations.get(&name) {
                let mut v = def.clone();
                v.ranges = merge_ranges(ranges);
                st.sheet.validations.push(v);
            }
        }
        self.images.extend(st.frames.drain(..).map(|f| (index, f)));
        self.wb.sheets.push(Arc::new(st.sheet));
    }

    /// The workbook, once its content is read: names resolved, then its view settings,
    /// properties and pictures (from `package`, or inline). A part that failed to parse warns.
    fn finish(
        mut self,
        settings: Option<Result<Elem, IoError>>,
        meta: Option<Result<Elem, IoError>>,
        package: Option<&mut Package<'_>>,
    ) -> Result<(Workbook, ReadReport), IoError> {
        if !self.spreadsheet_seen {
            return Err(xml_error("incomplete ODS document"));
        }
        if self.wb.sheets.is_empty() {
            return Err(IoError::Format("ODS contains no worksheets".into()));
        }
        self.finish_names();
        match settings {
            Some(Ok(tree)) => settings::apply(&tree, &mut self.wb),
            Some(Err(_)) => self.report.warn("The ODS view settings are damaged; default views were used."),
            None => {}
        }
        match meta {
            Some(Ok(tree)) => read_meta(&tree, &mut self.wb),
            Some(Err(_)) => self.report.warn("The ODS document properties are damaged and were not imported."),
            None => {}
        }
        self.load_pictures(package);
        self.rename_sheets();
        Ok((self.wb, self.report))
    }

    /// Gives each sheet a name GridCraft can hold (valid, unique, at most 31 characters) and
    /// renames the references to it, all at once, once the file is read.
    fn rename_sheets(&mut self) {
        let mut names = crate::read::SheetNames::default();
        // Each name the file uses, folded as sheet names compare, to the final name of the
        // first sheet it calls so: the one its references mean.
        let mut renamed: HashMap<String, String> = HashMap::new();
        let mut any = false;
        for i in 0..self.wb.sheets.len() {
            let Some(old) = self.wb.sheets.get(i).map(|s| s.name.clone()) else { continue };
            let mut new = names.unique(&old, i);
            if new.eq_ignore_ascii_case("History") || new.starts_with('\'') || new.ends_with('\'') {
                new = names.unique(&format!("Sheet{}", i + 1), i);
            }
            renamed.entry(old.to_ascii_lowercase()).or_insert_with(|| new.clone());
            if new != old {
                any = true;
                if let Some(sh) = self.wb.sheets.get_mut(i) {
                    Arc::make_mut(sh).name = new;
                }
            }
        }
        if !any {
            return;
        }
        self.report.warn("Some ODS sheet names were shortened or renamed to valid, unique sheet names.");
        let map = |n: &str| renamed.get(&n.to_ascii_lowercase()).cloned();
        self.wb.rewrite_references(&|_, e| gridcraft_formula::adjust::rename_sheets(e, &map));
    }

    /// The pivot tables of a `table:data-pilot-tables`, placed on their sheets, numbered over
    /// the workbook and within [`MAX_PIVOTS`].
    fn pivot_tables(&mut self, el: &Elem) {
        let mut pivots = el.children(Ns::Table, "data-pilot-table").peekable();
        if pivots.peek().is_none() {
            return;
        }
        for p in pivots {
            if self.pivots >= MAX_PIVOTS {
                self.report.warn("Some ODS pivot tables beyond the import limit were imported as their current values.");
                return;
            }
            let id = self.pivots as u32 + 1;
            let placed = super::pivot::read(p, id).and_then(|(sheet, pt)| {
                let i = self.wb.sheets.iter().position(|s| s.name == sheet)?;
                let s = self.wb.sheets.get_mut(i)?;
                Arc::make_mut(s).pivots.push(pt);
                Some(())
            });
            match placed {
                Some(()) => self.pivots += 1,
                None => self.report.warn("Some ODS pivot tables (from databases or external sources) were imported as their current values."),
            }
        }
    }

    /// Whether `e` may be kept for later, within [`MAX_KEPT_NODES`] over the document.
    fn keep(&mut self, e: &Elem) -> bool {
        self.kept_nodes = self.kept_nodes.saturating_add(e.size());
        let keep = self.kept_nodes <= MAX_KEPT_NODES;
        if !keep {
            self.report.warn("Some ODS page styles, database ranges or charts beyond the import limit were not imported.");
        }
        keep
    }

    fn read_master_pages(&mut self, m: &Elem) {
        for p in m.children(Ns::Style, "master-page") {
            if let Some(n) = p.attr(Ns::Style, "name")
                && self.keep(p)
            {
                self.master_pages.insert(n.to_string(), p.clone());
            }
        }
    }

    fn sheet_index(&self) -> Option<usize> {
        self.sheet.as_ref().map(|_| self.wb.sheets.len())
    }

    fn named_expressions(&mut self, e: &Elem) {
        let scope = self.sheet_index();
        for n in e.elems() {
            let Some(name) = n.attr(Ns::Table, "name") else { continue };
            let range = n.is(Ns::Table, "named-range");
            let text = match (range, n.is(Ns::Table, "named-expression")) {
                (true, _) => n.attr(Ns::Table, "cell-range-address"),
                (false, true) => n.attr(Ns::Table, "expression"),
                _ => None,
            };
            if let Some(text) = text {
                let base = n.attr(Ns::Table, "base-cell-address").map(str::to_string);
                self.names.push(NameDef { name: name.to_string(), scope, text: text.to_string(), range, base });
            }
        }
    }

    pub(crate) fn finish_names(&mut self) {
        // The names already defined, by scope and folded name: one definition per name and scope.
        let mut defined: std::collections::HashSet<(Option<usize>, String)> =
            self.wb.names.iter().map(|n| (n.scope, n.name.to_lowercase())).collect();
        for NameDef { name, scope, text, range, base } in std::mem::take(&mut self.names) {
            let formula = if range {
                // `$Sheet1.$A$1:.$B$2`, possibly several ranges separated by spaces.
                formula::range_list(&text, false, |r| Some(gridcraft_formula::printer::reference_a1(&r))).unwrap_or_else(|| "#REF!".into())
            } else {
                // An expression that no longer reads (`$#REF!.$C$17`, a deleted sheet) is kept
                // as the error it calculates to, as LibreOffice keeps it.
                formula::from_odf(&text).unwrap_or_else(|| {
                    if !text.contains("#REF") {
                        self.report.warn("Some ODS named expressions use syntax GridCraft doesn't read; they were kept as #REF!.");
                    }
                    "#REF!".into()
                })
            };
            // A reference that no longer reads (a deleted sheet) is an error, as it calculates.
            let formula = if gridcraft_formula::parse(&formula).is_ok() { formula } else { "#REF!".to_string() };
            // Relative references are relative to the base cell in ODF, to A1 in the model.
            let formula = match base.as_deref().and_then(formula::parse_odf_ref).map(|r| r.range().start) {
                Some(b) if b != CellRef::new(0, 0) => rebase_relative(&formula, b),
                _ => formula,
            };
            // Names that clash with the workbook's own rules are kept only when valid.
            if gridcraft_formula::parse_name(&name).is_err() && !name.starts_with('_') {
                self.report.warn("Some ODS named ranges have names GridCraft can't use and were skipped.");
                continue;
            }
            // A name defined twice in one scope keeps its first definition.
            if !defined.insert((scope, name.to_lowercase())) {
                continue;
            }
            self.wb.names.push(DefinedName { name, scope, formula, comment: String::new(), hidden: false });
        }
        // A database range's name must be new in every scope.
        let mut taken: std::collections::HashSet<String> = defined.into_iter().map(|(_, n)| n).collect();
        let sheets: HashMap<String, usize> = self.wb.sheets.iter().enumerate().rev().map(|(i, s)| (s.name.clone(), i)).collect();
        for db in std::mem::take(&mut self.database_ranges) {
            self.database_range(&db, &sheets, &mut taken);
        }
    }

    /// A database range: a defined name when its name isn't `taken` (folded) in any scope, and
    /// the sheet's autofilter when it shows filter buttons.
    fn database_range(&mut self, db: &Elem, sheets: &HashMap<String, usize>, taken: &mut std::collections::HashSet<String>) {
        let Some(addr) = db.attr(Ns::Table, "target-range-address") else { return };
        let (sheet, range) = match formula::range_on_sheet(addr) {
            Some(v) => v,
            None => return,
        };
        // A named database range is usable in formulas by its name.
        if let Some(name) = db.attr(Ns::Table, "name")
            && !name.starts_with("__Anonymous")
            && gridcraft_formula::parse_name(name).is_ok()
            && sheets.contains_key(&sheet)
            && taken.insert(name.to_lowercase())
        {
            let r = formula::absolute(&sheet, range, false);
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
        let Some(&idx) = sheets.get(&sheet) else { return };
        // An autofilter holds one criterion per column of its range.
        let width = range.width();
        let mut by_field: BTreeMap<u32, FilterCriterion> = BTreeMap::new();
        let (mut outside, mut unread) = (false, false);
        // Whether a criterion was kept: false for a column's condition beyond what it holds.
        let mut add = |field: u32, crit: FilterCriterion| -> bool {
            if field >= width {
                outside = true;
                return true;
            }
            match by_field.entry(field) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(crit);
                    true
                }
                // A second custom condition on a column is ANDed with the first; a column holds
                // no more than that.
                std::collections::btree_map::Entry::Occupied(mut o) => match (o.get_mut(), crit) {
                    (FilterCriterion::Custom { b: b @ None, .. }, FilterCriterion::Custom { a, b: None, .. }) => {
                        *b = Some(a);
                        true
                    }
                    _ => false,
                },
            }
        };
        if let Some(filter) = db.child(Ns::Table, "filter") {
            // A filter is an AND of conditions, each one alone or an OR of two on one column (as
            // an autofilter column's custom filter is).
            let groups = match filter.elems().next() {
                Some(g) if g.is(Ns::Table, "filter-and") => g.elems().collect(),
                Some(g) => vec![g],
                None => Vec::new(),
            };
            for g in groups {
                let read = if g.is(Ns::Table, "filter-condition") {
                    filter_criterion(g).is_some_and(|(field, crit)| add(field, crit))
                } else if g.is(Ns::Table, "filter-or") {
                    // Every condition of the OR, or it isn't read: one left out would widen it.
                    let conds: Option<Vec<(u32, FilterCriterion)>> =
                        if g.elems().all(|c| c.is(Ns::Table, "filter-condition")) { g.elems().take(3).map(filter_criterion).collect() } else { None };
                    match conds.as_deref().unwrap_or_default() {
                        [(f, FilterCriterion::Custom { a, .. }), (f2, FilterCriterion::Custom { a: b, .. })] if f == f2 => {
                            add(*f, FilterCriterion::Custom { a: a.clone(), b: Some(b.clone()), and: false })
                        }
                        [one] => add(one.0, one.1.clone()),
                        _ => false,
                    }
                } else {
                    true
                };
                unread |= !read;
            }
        }
        if unread {
            self.report.warn("Some ODS filters combine conditions in ways GridCraft doesn't read; they were left out.");
        }
        // A criterion for a column outside the range has nothing to filter.
        if outside {
            self.report.warn("Some ODS filter conditions name columns outside their range and were not imported.");
        }
        let criteria: Vec<(u32, FilterCriterion)> = by_field.into_iter().collect();
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
            // Its messages and formulas count as they are decoded; past the limit it is left out.
            let bytes = v.f1.len() + v.f2.as_ref().map_or(0, String::len) + v.input_message.len() + v.error_message.len();
            if self.take_text(bytes) {
                self.validations.insert(name.to_string(), v);
            }
        }
    }

    fn conditional_formats(&mut self, e: &Elem) {
        let mut out = Vec::new();
        let mut priority = 1u32;
        for cf in e.children(Ns::CalcExt, "conditional-format") {
            let Some(addr) = cf.attr(Ns::CalcExt, "target-range-address") else { continue };
            let ranges: Vec<RangeRef> = formula::split_addresses(addr).into_iter().filter_map(formula::parse_odf_ref).map(|r| r.range()).collect();
            if ranges.is_empty() {
                continue;
            }
            for rule in cf.elems() {
                if let Some(r) = self.cf_rule(rule) {
                    if !self.take_cf_ranges(ranges.len()) {
                        break;
                    }
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

    /// Counts `n` bytes of decoded text; past the import's text limit the file is refused.
    fn charge_text(&mut self, n: usize) -> Result<(), IoError> {
        self.text = self.text.saturating_add(n as u64);
        if self.text > MAX_TEXT {
            return Err(IoError::TooLarge("ODS expands beyond the import limit (256 MiB of text)".into()));
        }
        Ok(())
    }

    /// Charges `n` bytes of text that may be left out to the import's text budget: false (with
    /// a warning, and nothing charged) when they don't fit, so the import goes on without them.
    fn take_text(&mut self, n: usize) -> bool {
        let total = self.text.saturating_add(n as u64);
        if total > MAX_TEXT {
            self.report.warn("Some ODS pictures' text, conditional formatting or data validation beyond the import limit were not imported.");
            return false;
        }
        self.text = total;
        true
    }

    /// Takes `n` conditional-format ranges from the import's budget; false (with a warning) when
    /// it is spent.
    fn take_cf_ranges(&mut self, n: usize) -> bool {
        self.cf_ranges = self.cf_ranges.saturating_add(n as u64);
        let ok = self.cf_ranges <= MAX_CF_RANGES;
        if !ok {
            self.report.warn("Some ODS conditional formatting beyond the import limit was not imported.");
        }
        ok
    }

    /// ODF's own conditional styles (`style:map` on a cell style), for files without
    /// LibreOffice's richer `calcext:conditional-formats`.
    fn style_map_formats(&mut self, st: &mut SheetState) {
        if st.has_calcext_formats {
            return;
        }
        let mut names: Vec<_> = st.mapped.drain().collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        let mut priority = st.sheet.cond_formats.len() as u32 + 1;
        for (name, ranges) in names {
            let Some(maps) = self.style_def(&name).map(|d| d.maps.clone()) else { continue };
            let ranges = merge_ranges(ranges);
            for (cond, apply, _) in &maps {
                // Each condition tried counts, read or not, and its formula text is the sheet's.
                if !self.take_cf_ranges(ranges.len()) || !self.take_text(cond.len()) {
                    return;
                }
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
        self.read_drawings(shapes, None);
    }

    /// Reads the drawings under `parent` (anchored to `cell`, or the sheet), within the limits:
    /// no more than [`MAX_FRAMES`] are read at all, and their text counts as it is decoded.
    fn read_drawings(&mut self, parent: &Elem, cell: Option<CellRef>) {
        let room = MAX_FRAMES.saturating_sub(self.frames);
        let mut text_left = usize::try_from(MAX_TEXT.saturating_sub(self.text)).unwrap_or(usize::MAX);
        let before = text_left;
        let mut frames = Vec::new();
        let full = drawings(parent, cell, &self.draw, &mut frames, room, &mut text_left);
        self.text = self.text.saturating_add((before - text_left) as u64);
        if full {
            self.report.warn("Some ODS pictures, charts and shapes, or their text, beyond the import limit were not imported.");
        }
        self.add_frames(frames);
    }

    /// Adds pictures, charts and shapes to the sheet, up to the workbook's [`MAX_FRAMES`].
    /// (They come from [`Import::read_drawings`], which reads no more than there is room for.)
    fn add_frames(&mut self, mut frames: Vec<Frame>) {
        // A flat document's chart is kept as its document until the pictures are loaded; one
        // past the kept-node limit is left out.
        frames.retain(|f| match &f.inline {
            Some(Inline::Chart(doc)) => self.keep(doc),
            _ => true,
        });
        self.frames += frames.len();
        if let Some(st) = self.sheet.as_mut() {
            st.frames.extend(frames);
        }
    }

    fn load_pictures(&mut self, mut package: Option<&mut Package<'_>>) {
        let mut next_id = 1u32;
        let mut next_chart = 1u32;
        let mut next_shape = 1u32;
        let mut lines = SheetLines::default();
        for (index, mut f) in std::mem::take(&mut self.images) {
            if let Some(d) = f.shape.take() {
                let Some(sheet) = self.wb.sheets.get_mut(index) else { continue };
                let sheet = Arc::make_mut(sheet);
                let anchor = place_anchor(lines.get(index, sheet), &f);
                sheet.shapes.push(gridcraft_model::Shape {
                    id: next_shape,
                    kind: d.kind,
                    anchor,
                    fill: d.fill,
                    line: d.line,
                    text: d.text,
                    format: d.format,
                    math: Default::default(),
                });
                next_shape += 1;
                continue;
            }
            let path = f.href.trim_start_matches("./").to_string();
            if f.object {
                let draft = match (f.inline.take(), package.as_deref_mut()) {
                    (Some(Inline::Chart(doc)), _) => super::chart::read(&doc),
                    (_, Some(package)) => {
                        let part = format!("{}/content.xml", path.trim_end_matches('/'));
                        match package.read_limited(&part, 64 * 1024 * 1024) {
                            Ok(Some(xml)) => dom::parse_tree(&xml).ok().as_ref().and_then(super::chart::read),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let Some(d) = draft else {
                    self.report.warn("Some ODS embedded objects that are not charts (or damaged charts) were not imported.");
                    continue;
                };
                let Some(sheet) = self.wb.sheets.get_mut(index) else { continue };
                let sheet = Arc::make_mut(sheet);
                let anchor = place_anchor(lines.get(index, sheet), &f);
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
            let data = match (f.inline.take(), package.as_deref_mut()) {
                (Some(Inline::Picture(bytes)), _) => Some(bytes),
                (_, Some(package)) if !path.is_empty() => package.read_limited(&path, 64 * 1024 * 1024).ok().flatten(),
                _ => None,
            };
            let Some(data) = data else {
                self.report.warn("Some ODS pictures are linked or missing and were not imported.");
                continue;
            };
            // The picture's kind from its bytes, or its name when they don't say.
            let mime = if data.starts_with(b"\x89PNG") {
                "image/png"
            } else if data.starts_with(&[0xFF, 0xD8]) {
                "image/jpeg"
            } else if data.starts_with(b"GIF8") {
                "image/gif"
            } else {
                match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
                    Some("png") => "image/png",
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("gif") => "image/gif",
                    _ => {
                        self.report.warn("Some ODS pictures in formats GridCraft doesn't show were not imported.");
                        continue;
                    }
                }
            };
            let Some(sheet) = self.wb.sheets.get_mut(index) else { continue };
            let sheet = Arc::make_mut(sheet);
            let anchor = place_anchor(lines.get(index, sheet), &f);
            sheet.images.push(Image { id: next_id, anchor, data: Arc::new(data), mime: mime.into(), alt: f.alt.clone() });
            next_id += 1;
        }
    }
}

/// ODF filter operators that are Excel comparisons with wildcards: the ODF operator, Excel's,
/// and whether a `*` stands before and after the value (`contains` is `=*x*`).
pub(crate) const WILDCARD_OPS: [(&str, &str, bool, bool); 6] = [
    ("contains", "=", true, true),
    ("!contains", "<>", true, true),
    ("begins", "=", false, true),
    ("!begins", "<>", false, true),
    ("ends", "=", true, false),
    ("!ends", "<>", true, false),
];

/// One `table:filter-condition` as the column it filters and an autofilter criterion.
fn filter_criterion(cond: &Elem) -> Option<(u32, FilterCriterion)> {
    let field = cond.attr(Ns::Table, "field-number").and_then(|f| f.parse::<u32>().ok())?;
    let op = cond.attr(Ns::Table, "operator").unwrap_or("=");
    let value = cond.attr(Ns::Table, "value").unwrap_or_default().to_string();
    let set: Vec<String> = cond.children(Ns::Table, "filter-set-item").filter_map(|i| i.attr(Ns::Table, "value").map(str::to_string)).collect();
    let crit = match op {
        // An empty item in a value list is the "(Blanks)" choice.
        "=" if !set.is_empty() => {
            let blanks = set.iter().any(String::is_empty);
            FilterCriterion::Values { values: set.into_iter().filter(|v| !v.is_empty()).collect(), blanks }
        }
        // A bare `=` is a comparison (a value list lists its items).
        "=" => FilterCriterion::Custom { a: ("=".into(), value), b: None, and: true },
        "empty" => FilterCriterion::Values { values: vec![], blanks: true },
        "top values" | "bottom values" | "top percent" | "bottom percent" => FilterCriterion::Top10 {
            bottom: op.starts_with("bottom"),
            percent: op.ends_with("percent"),
            count: value.parse::<f64>().ok().map_or(10, |v| v.clamp(1.0, 500.0) as u32),
        },
        o => {
            let (op, v) = match WILDCARD_OPS.iter().find(|(odf, ..)| *odf == o) {
                Some((_, op, before, after)) => (*op, format!("{}{value}{}", if *before { "*" } else { "" }, if *after { "*" } else { "" })),
                None => match o {
                    "!=" => ("<>", value),
                    "<" | ">" | "<=" | ">=" => (o, value),
                    _ => ("=", value),
                },
            };
            FilterCriterion::Custom { a: (op.to_string(), v), b: None, and: true }
        }
    };
    Some((field, crit))
}

/// The offsets of a sheet's columns or rows in points, for placing drawings: the lines with an
/// entry, each with where it starts and its size, and the size of the lines without one.
pub(crate) struct Lines {
    entries: Vec<(u32, f32, f32)>,
    default: f32,
    max: u32,
}

impl Lines {
    fn new(map: &BTreeMap<u32, LineInfo>, default_px: f32, max: u32) -> Lines {
        let default = default_px * 0.75;
        let mut entries = Vec::with_capacity(map.len());
        let (mut next, mut at) = (0u32, 0f32);
        for (&i, info) in map.range(..max) {
            at += (i - next) as f32 * default;
            let size = if info.hidden { 0.0 } else { info.size.map_or(default, |s| s * 0.75) };
            entries.push((i, at, size));
            at += size;
            next = i + 1;
        }
        Lines { entries, default, max }
    }

    /// Where line `i` starts.
    fn offset(&self, i: u32) -> f32 {
        let k = self.entries.partition_point(|e| e.0 < i);
        match k.checked_sub(1).and_then(|k| self.entries.get(k)) {
            Some(&(j, start, size)) => start + size + (i - j - 1) as f32 * self.default,
            None => i as f32 * self.default,
        }
    }

    /// The line holding position `pos`, and how far into it `pos` is.
    fn locate(&self, pos: f32) -> (u32, f32) {
        let k = self.entries.partition_point(|e| e.1 <= pos);
        let (base, base_at) = match k.checked_sub(1).and_then(|k| self.entries.get(k)) {
            Some(&(j, start, size)) if pos < start + size => return (j, pos - start),
            Some(&(j, start, size)) => (j + 1, start + size),
            None => (0, 0.0),
        };
        let steps = if self.default > 0.0 { ((pos - base_at) / self.default).floor().max(0.0) } else { 0.0 };
        let line = (base as f64 + f64::from(steps)).min(f64::from(self.max - 1)) as u32;
        (line, pos - self.offset(line))
    }
}

/// Each sheet's column and row offsets, made when a drawing on it is first placed.
#[derive(Default)]
struct SheetLines(HashMap<usize, (Lines, Lines)>);

impl SheetLines {
    fn get(&mut self, index: usize, sheet: &Sheet) -> (&Lines, &Lines) {
        let (c, r) = self.0.entry(index).or_insert_with(|| sheet_lines(sheet));
        (c, r)
    }
}

/// The anchor of a picture: its cell and offset from the sheet origin or its anchor cell.
fn place_anchor((cols, rows): (&Lines, &Lines), f: &Frame) -> Anchor {
    let (x, y) = (f.x as f32, f.y as f32);
    let (cell, dx, dy) = match f.cell {
        Some(c) => (c, x - cols.offset(c.col), y - rows.offset(c.row)),
        None => {
            let (col, dx) = cols.locate(x);
            let (row, dy) = rows.locate(y);
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

/// A sheet's column and row offsets.
pub(crate) fn sheet_lines(sheet: &Sheet) -> (Lines, Lines) {
    (Lines::new(&sheet.cols, sheet.default_col_width, MAX_COLS), Lines::new(&sheet.rows, sheet.default_row_height, MAX_ROWS))
}

/// Where an anchor's top-left corner is on its sheet (with offsets `lines`), in points
/// (anchors are in pixels).
pub(crate) fn anchor_pt((cols, rows): &(Lines, Lines), a: &Anchor) -> (f32, f32) {
    (cols.offset(a.cell.col) + a.dx * 0.75, rows.offset(a.cell.row) + a.dy * 0.75)
}

fn read_frame(f: &Elem, cell: Option<CellRef>) -> Option<Frame> {
    let img = f.child(Ns::Draw, "image");
    let obj = f.child(Ns::Draw, "object");
    let len = |n: &str| f.attr(Ns::Svg, n).and_then(length_pt).unwrap_or(0.0);
    // A flat document holds a chart's document, or a picture's bytes, inline.
    let inline = match (obj.and_then(|o| o.child(Ns::Office, "document")), img.and_then(|i| i.child(Ns::Office, "binary-data"))) {
        (Some(doc), _) => Some(Inline::Chart(Box::new(doc.clone()))),
        (None, Some(data)) => gridcraft_model::b64::decode_limited(&data.raw_text(), 64 * 1024 * 1024).map(Inline::Picture),
        _ => None,
    };
    let frame = Frame { cell, x: len("x"), y: len("y"), width: len("width"), height: len("height"), ..Frame::default() };
    if let Some(inline) = inline {
        let object = matches!(inline, Inline::Chart(_));
        let alt = f.child(Ns::Svg, "title").map(Elem::raw_text).unwrap_or_default();
        return Some(Frame { object, inline: Some(inline), alt, ..frame });
    }
    let href = img.or(obj).and_then(|i| i.attr(Ns::XLink, "href"))?.to_string();
    let alt = f.child(Ns::Svg, "title").map(Elem::raw_text).or_else(|| f.child(Ns::Svg, "desc").map(Elem::raw_text)).unwrap_or_default();
    // An embedded object (a chart) comes with a replacement picture that is only its preview.
    let (href, object) = match obj.and_then(|o| o.attr(Ns::XLink, "href")) {
        Some(o) => (o.to_string(), true),
        None => (href, false),
    };
    Some(Frame { href, object, alt, ..frame })
}

/// Pictures, charts and shapes among `parent`'s children, groups flattened.
/// The drawings under `parent`, at most `room` of them, their shapes' text taken from
/// `text_left` as it is decoded. Whether a limit cut them short.
fn drawings(
    parent: &Elem,
    cell: Option<CellRef>,
    styles: &super::shape::DrawStyles,
    out: &mut Vec<Frame>,
    room: usize,
    text_left: &mut usize,
) -> bool {
    let mut stack: Vec<(&Elem, usize)> = parent.elems().filter(|e| e.ns == Ns::Draw).map(|e| (e, 0)).collect();
    stack.reverse();
    let mut cut = false;
    while let Some((e, depth)) = stack.pop() {
        if e.name == "g" {
            if depth < 16 {
                stack.extend(e.elems().filter(|k| k.ns == Ns::Draw).map(|k| (k, depth + 1)).collect::<Vec<_>>().into_iter().rev());
            }
            continue;
        }
        if out.len() >= room {
            return true;
        }
        if e.name == "frame"
            && let Some(f) = read_frame(e, cell)
        {
            out.push(f);
            continue;
        }
        let cap = (*text_left).min(dom::MAX_CELL_TEXT);
        if let Some(d) = super::shape::read(e, styles, cap) {
            // Text as long as what was left of the budget was cut at it.
            cut |= cap < dom::MAX_CELL_TEXT && d.text.len() >= cap && !e.descendants(Ns::Text, "p").is_empty();
            *text_left = text_left.saturating_sub(d.text.len());
            out.push(Frame { cell, x: d.x, y: d.y, width: d.width, height: d.height, shape: Some(d), ..Frame::default() });
        }
    }
    cut
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
/// `r` with every anchor relative (a link names a place, not a fixed reference).
fn relative(mut r: gridcraft_formula::Reference) -> gridcraft_formula::Reference {
    use gridcraft_formula::RefKind;
    let rel = |a: &mut gridcraft_formula::Anchor| {
        a.row_abs = false;
        a.col_abs = false;
    };
    match &mut r.kind {
        RefKind::Cell(a) => rel(a),
        RefKind::Range(a, b) => {
            rel(a);
            rel(b);
        }
        RefKind::Cols(_, a0, _, a1) | RefKind::Rows(_, a0, _, a1) => {
            *a0 = false;
            *a1 = false;
        }
    }
    r
}

fn hyperlink(href: &str, title: Option<&str>) -> Hyperlink {
    let tooltip = title.map(str::to_string);
    if let Some(local) = href.strip_prefix('#') {
        let target = match formula::parse_odf_ref(local) {
            Some(r) => gridcraft_formula::printer::reference_a1(&relative(r)),
            // A sheet alone (`#Sheet2`) is its first cell.
            _ if !local.contains('.') => format!("{}!A1", gridcraft_formula::quote_sheet(local.trim_matches('\''))),
            _ => local.to_string(),
        };
        return Hyperlink { target, tooltip, external: false };
    }
    // A relative link is relative to the package as a folder (ODF 1.3 part 2 §3.7): `../x.ods`
    // is next to the file, which is where the model's relative links start.
    let target = if is_relative(href) { href.strip_prefix("../").unwrap_or(href).to_string() } else { href.to_string() };
    Hyperlink { target, tooltip, external: true }
}

/// A link that is neither a URL with a scheme (nor a drive, `C:`) nor an absolute path.
pub(crate) fn is_relative(href: &str) -> bool {
    !href.starts_with('#') && crate::passthrough::is_relative_path(href)
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

/// Whether two row heights or column widths (in pixels) are the same size.
fn same_size(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

/// Stores `info` for each line of `run` below `max`, taking one entry per line from `budget`;
/// false when the budget ran out.
fn store_run(lines: &mut BTreeMap<u32, LineInfo>, run: &LineRun, max: u32, info: LineInfo, budget: &mut usize) -> bool {
    for i in run.start..run.start.saturating_add(run.count).min(max) {
        let Some(left) = budget.checked_sub(1) else { return false };
        *budget = left;
        lines.insert(i, info);
    }
    true
}

/// The size most lines have, sizes counted to a hundredth of a pixel (each as first seen).
fn most_common(items: impl Iterator<Item = (Option<f32>, u32)>) -> Option<f32> {
    let mut counts: HashMap<i64, (usize, f32, u64)> = HashMap::new();
    for (size, n) in items {
        let Some(s) = size.filter(|s| s.is_finite()) else { continue };
        let seen = counts.len();
        counts.entry((f64::from(s) * 100.0).round() as i64).or_insert((seen, s, 0)).2 += u64::from(n);
    }
    // Ties go to the size seen last.
    counts.into_values().max_by_key(|&(first, _, c)| (c, first)).map(|(_, v, _)| v)
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
    // A range test, on the content or on the text's length.
    for (name, op, textlen) in [
        ("cell-content-is-between", CfOperator::Between, false),
        ("cell-content-is-not-between", CfOperator::NotBetween, false),
        ("cell-content-text-length-is-between", CfOperator::Between, true),
        ("cell-content-text-length-is-not-between", CfOperator::NotBetween, true),
    ] {
        if let Some((args, _)) = call(rest, name) {
            if textlen {
                v.kind = ValidationKind::TextLength;
            }
            v.op = op;
            v.f1 = args.first().map(|a| expr_text(a)).unwrap_or_default();
            v.f2 = args.get(1).map(|a| expr_text(a));
            return;
        }
    }
    // A comparison, on the text's length or on the content.
    let rest_norm = match rest.strip_prefix("cell-content-text-length()") {
        Some(r) => {
            v.kind = ValidationKind::TextLength;
            r.trim()
        }
        None => rest,
    };
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
    fn capture(&mut self, ns: Ns, name: &str) -> bool {
        if self.skip_depth.is_some() {
            return false;
        }
        let in_table = self.sheet.is_some();
        match (ns, name) {
            (
                Ns::Office,
                "font-face-decls" | "automatic-styles" | "styles" | "scripts" | "forms" | "event-listeners" | "master-styles" | "meta" | "settings",
            ) => true,
            (Ns::Table, "table-column" | "table-row" | "shapes" | "table-source" | "scenario" | "dde-source") if in_table => true,
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
            _ => false,
        }
    }

    fn open(&mut self, el: &Elem) -> Result<(), IoError> {
        if self.depth == 0 && !(el.is(Ns::Office, "document-content") || (self.flat && el.is(Ns::Office, "document"))) {
            return Err(xml_error("expected one office:document-content root"));
        }
        self.depth += 1;
        if self.skip_depth.is_some() {
            return Ok(());
        }
        match (el.ns, el.name.as_str()) {
            (Ns::Office, "spreadsheet") => {
                if self.spreadsheet_seen {
                    return Err(xml_error("multiple spreadsheets in one document"));
                }
                self.spreadsheet_seen = true;
                if el.attr(Ns::Table, "structure-protected") == Some("true") {
                    self.wb.protected_structure = true;
                    self.wb.structure_password = self.protection_key(el);
                }
                self.ready_styles();
            }
            // An external reference's cached source table is named `'file:///…'#Sheet`.
            (Ns::Table, "table") if el.attr(Ns::Table, "name").is_some_and(|n| n.starts_with('\'') && n.contains("'#")) => {
                self.skip_depth = Some(self.depth);
            }
            (Ns::Table, "table") if self.spreadsheet_seen && self.sheet.is_none() => self.open_table(el)?,
            (Ns::Table, "table-row-group") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.row_group_hidden.push(el.attr(Ns::Table, "display") == Some("false"));
                }
            }
            (Ns::Table, "table-column-group") => {
                if let Some(st) = self.sheet.as_mut() {
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
        if let Some(d) = self.skip_depth {
            if d == self.depth {
                self.skip_depth = None;
            }
            self.depth = self.depth.saturating_sub(1);
            return Ok(());
        }
        self.depth = self.depth.saturating_sub(1);
        match (el.ns, el.name.as_str()) {
            (Ns::Table, "table") if self.sheet.as_ref().is_some_and(|s| s.depth == self.depth + 1) => self.finish_sheet(),
            (Ns::Table, "table-row-group") => {
                if let Some(st) = self.sheet.as_mut() {
                    st.row_group_hidden.pop();
                }
            }
            (Ns::Table, "table-column-group") => {
                if let Some(st) = self.sheet.as_mut() {
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
            (Ns::Office, "master-styles") => self.read_master_pages(&el),
            (Ns::Office, "meta") => self.flat_meta = Some(el),
            (Ns::Office, "settings") => self.flat_settings = Some(el),
            (Ns::Table, "table-column") => self.column(&el)?,
            (Ns::Table, "table-row") => {
                if self.sheet.is_some() {
                    self.row(&el)?;
                }
            }
            (Ns::Table, "named-expressions") => self.named_expressions(&el),
            (Ns::Table, "database-ranges") => {
                for r in el.children(Ns::Table, "database-range") {
                    if self.keep(r) {
                        self.database_ranges.push(r.clone());
                    }
                }
            }
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
            (Ns::Table, "data-pilot-tables") => self.pivot_tables(&el),
            // LibreOffice lists its (empty) Basic library in every file: only real code counts.
            (Ns::Office, "scripts") if el.descendants(Ns::Other, "module").len() + el.descendants(Ns::Other, "source-code").len() > 0 => {
                self.report.warn("ODS macros were not imported.");
            }
            _ => {}
        }
        Ok(())
    }
}

/// Whether `text` is how a cell shows an error: Excel's (`#DIV/0!`), one in another language
/// (`#VALORE!`, `#WERT!`) or LibreOffice's numbered ones (`Err:532`).
fn is_error_text(text: &str) -> bool {
    text.starts_with("Err:")
        || CellError::parse(text).is_some()
        || (text.starts_with('#') && (text.ends_with('!') || text.ends_with('?')) && text.len() <= 16)
}

/// The error a cell shows as `text` (see [`is_error_text`]); numbered ones map to the Excel
/// error LibreOffice itself saves them as.
fn error_value(text: &str) -> CellError {
    if let Some(e) = CellError::parse(text) {
        return e;
    }
    if text.starts_with('#') && (text.ends_with('!') || text.ends_with('?')) {
        return CellError::Value;
    }
    match text.strip_prefix("Err:").and_then(|n| n.parse::<u32>().ok()) {
        Some(524) => CellError::Ref,
        Some(525) => CellError::Name,
        Some(532) => CellError::Div0,
        Some(503 | 523) => CellError::Num,
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
