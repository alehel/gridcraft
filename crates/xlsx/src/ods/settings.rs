//! `settings.xml` (ODF 1.3 part 3 §3.10, with LibreOffice's `ooo:view-settings` items): the
//! active sheet, frozen panes, cursor, scroll position, zoom and display switches per sheet.

use std::fmt::Write as _;
use std::sync::Arc;

use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS};
use gridcraft_model::{SheetViewMode, Workbook};

use super::dom::{Elem, Ns};
use crate::xml::{esc, esc_attr};

fn item<'a>(e: &'a Elem, name: &str) -> Option<&'a str> {
    e.children(Ns::Config, "config-item")
        .find(|i| i.attr(Ns::Config, "name") == Some(name))
        .map(|i| i.kids.iter().find_map(|k| if let super::dom::Node::Text(t) = k { Some(t.as_str()) } else { None }).unwrap_or(""))
}

fn int(e: &Elem, name: &str) -> Option<i64> {
    item(e, name).and_then(|v| v.trim().parse::<i64>().ok())
}

fn boolean(e: &Elem, name: &str) -> Option<bool> {
    item(e, name).map(|v| v.trim() == "true")
}

fn clamp_u32(v: i64, max: u32) -> u32 {
    v.clamp(0, i64::from(max.saturating_sub(1))) as u32
}

pub(crate) fn apply(root: &Elem, wb: &mut Workbook) {
    let Some(settings) = root.child(Ns::Office, "settings") else { return };
    let Some(view) = settings.children(Ns::Config, "config-item-set").find(|s| s.attr(Ns::Config, "name") == Some("ooo:view-settings")) else {
        return;
    };
    let Some(views) = view.children(Ns::Config, "config-item-map-indexed").find(|m| m.attr(Ns::Config, "name") == Some("Views")) else { return };
    let Some(first) = views.child(Ns::Config, "config-item-map-entry") else { return };
    let show_grid = boolean(first, "ShowGrid");
    let show_headers = boolean(first, "HasColumnRowHeaders");
    let show_zeros = boolean(first, "ShowZeroValues");
    let show_tabs = boolean(first, "HasSheetTabs");
    let page_break_view = boolean(first, "ShowPageBreakPreview").unwrap_or(false);
    if let Some(active) = item(first, "ActiveTable")
        && let Some(i) = wb.sheets.iter().position(|s| s.name == active)
    {
        wb.set_active_sheet(i);
    }
    if show_tabs == Some(false) {
        wb.view.show_sheet_tabs = false;
    }
    let tables = first.children(Ns::Config, "config-item-map-named").find(|m| m.attr(Ns::Config, "name") == Some("Tables"));
    for sheet in wb.sheets.iter_mut() {
        let s = Arc::make_mut(sheet);
        if let Some(g) = show_grid {
            s.show_gridlines = g;
        }
        if let Some(h) = show_headers {
            s.show_headings = h;
        }
        if let Some(z) = show_zeros {
            s.show_zeros = z;
        }
        if page_break_view {
            s.view = SheetViewMode::PageBreakPreview;
        }
        let Some(entry) =
            tables.and_then(|t| t.children(Ns::Config, "config-item-map-entry").find(|e| e.attr(Ns::Config, "name") == Some(s.name.as_str())))
        else {
            continue;
        };
        let x = int(entry, "CursorPositionX").unwrap_or(0);
        let y = int(entry, "CursorPositionY").unwrap_or(0);
        s.view_active = CellRef::new(clamp_u32(y, MAX_ROWS), clamp_u32(x, MAX_COLS));
        // Split mode 2 is frozen; the position is the number of columns/rows frozen.
        let hmode = int(entry, "HorizontalSplitMode").unwrap_or(0);
        let vmode = int(entry, "VerticalSplitMode").unwrap_or(0);
        let cols = if hmode == 2 { int(entry, "HorizontalSplitPosition").unwrap_or(0) } else { 0 };
        let rows = if vmode == 2 { int(entry, "VerticalSplitPosition").unwrap_or(0) } else { 0 };
        if cols > 0 || rows > 0 {
            s.freeze = Some((clamp_u32(rows, MAX_ROWS), clamp_u32(cols, MAX_COLS)));
        }
        let left = if hmode == 2 { int(entry, "PositionRight") } else { int(entry, "PositionLeft") }.unwrap_or(0);
        let top = if vmode == 2 { int(entry, "PositionBottom") } else { int(entry, "PositionTop") }.unwrap_or(0);
        s.view_top_left = CellRef::new(clamp_u32(top, MAX_ROWS), clamp_u32(left, MAX_COLS));
        if let Some(z) = int(entry, "ZoomValue") {
            s.zoom = z.clamp(10, 400) as u16;
        }
        if let Some(g) = boolean(entry, "ShowGrid") {
            s.show_gridlines = g;
        }
        if let Some(c) = item(entry, "TabColor").and_then(|v| v.trim().parse::<i64>().ok())
            && (0..=0xFF_FFFF).contains(&c)
            && s.tab_color.is_none()
        {
            s.tab_color = Some(gridcraft_model::Color::Rgb(c as u32));
        }
    }
}

fn push_item(out: &mut String, name: &str, ty: &str, value: &str) {
    let _ = write!(out, "<config:config-item config:name=\"{}\" config:type=\"{ty}\">{}</config:config-item>", esc_attr(name), esc(value));
}

/// The `office:settings` of `wb`.
pub(crate) fn write(wb: &Workbook) -> String {
    let mut o = String::from(
        r#"<office:settings><config:config-item-set config:name="ooo:view-settings"><config:config-item-map-indexed config:name="Views"><config:config-item-map-entry>"#,
    );
    push_item(&mut o, "ViewId", "string", "view1");
    o.push_str("<config:config-item-map-named config:name=\"Tables\">");
    for s in &wb.sheets {
        let _ = write!(o, "<config:config-item-map-entry config:name=\"{}\">", esc_attr(&s.name));
        push_item(&mut o, "CursorPositionX", "int", &s.view_active.col.to_string());
        push_item(&mut o, "CursorPositionY", "int", &s.view_active.row.to_string());
        let (rows, cols) = s.freeze.unwrap_or((0, 0));
        push_item(&mut o, "HorizontalSplitMode", "short", if cols > 0 { "2" } else { "0" });
        push_item(&mut o, "VerticalSplitMode", "short", if rows > 0 { "2" } else { "0" });
        push_item(&mut o, "HorizontalSplitPosition", "int", &cols.to_string());
        push_item(&mut o, "VerticalSplitPosition", "int", &rows.to_string());
        push_item(&mut o, "ActiveSplitRange", "short", if rows > 0 || cols > 0 { "3" } else { "2" });
        // The scroll position is the right/bottom pane's when the sheet is frozen that way, else
        // the left/top one's (as `read` takes it).
        let (left, right) = if cols > 0 { (0, s.view_top_left.col.max(cols)) } else { (s.view_top_left.col, 0) };
        let (top, bottom) = if rows > 0 { (0, s.view_top_left.row.max(rows)) } else { (s.view_top_left.row, 0) };
        push_item(&mut o, "PositionLeft", "int", &left.to_string());
        push_item(&mut o, "PositionRight", "int", &right.to_string());
        push_item(&mut o, "PositionTop", "int", &top.to_string());
        push_item(&mut o, "PositionBottom", "int", &bottom.to_string());
        push_item(&mut o, "ZoomType", "short", "0");
        push_item(&mut o, "ZoomValue", "int", &s.zoom.clamp(10, 400).to_string());
        push_item(&mut o, "ShowGrid", "boolean", if s.show_gridlines { "true" } else { "false" });
        o.push_str("</config:config-item-map-entry>");
    }
    o.push_str("</config:config-item-map-named>");
    let active = wb.sheets.get(wb.active_sheet()).map(|s| s.name.as_str()).unwrap_or_default();
    push_item(&mut o, "ActiveTable", "string", active);
    // LibreOffice keeps these per document, not per sheet: the first sheet's stand for all.
    let first = wb.sheets.first();
    push_item(&mut o, "ShowZeroValues", "boolean", if first.is_none_or(|s| s.show_zeros) { "true" } else { "false" });
    push_item(&mut o, "ShowGrid", "boolean", if first.is_none_or(|s| s.show_gridlines) { "true" } else { "false" });
    push_item(&mut o, "HasColumnRowHeaders", "boolean", if first.is_none_or(|s| s.show_headings) { "true" } else { "false" });
    push_item(&mut o, "HasSheetTabs", "boolean", if wb.view.show_sheet_tabs { "true" } else { "false" });
    push_item(
        &mut o,
        "ShowPageBreakPreview",
        "boolean",
        if first.is_some_and(|s| s.view == SheetViewMode::PageBreakPreview) { "true" } else { "false" },
    );
    o.push_str("</config:config-item-map-entry></config:config-item-map-indexed></config:config-item-set></office:settings>");
    o
}
