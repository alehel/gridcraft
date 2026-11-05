//! Pivot tables: `table:data-pilot-table` (ODF 1.3 part 3 §9.6) ↔ GridCraft PivotTables — the
//! source range, row, column, page and data fields with their functions, sorting, "show as"
//! references, date grouping, layout and grand totals. The report's cells are ordinary cells.

use std::fmt::Write as _;

use gridcraft_core::{CellRef, RangeRef};
use gridcraft_model::{PivotDateGroup, PivotField, PivotFilter, PivotFunc, PivotLayout, PivotShowAs, PivotSort, PivotTable, PivotValue, Workbook};

use super::dom::{Elem, Ns};
use super::formula;
use crate::xml::esc_attr;

fn func(s: &str) -> PivotFunc {
    match s {
        "count" => PivotFunc::Count,
        "average" => PivotFunc::Average,
        "max" => PivotFunc::Max,
        "min" => PivotFunc::Min,
        "product" => PivotFunc::Product,
        "countnums" => PivotFunc::CountNumbers,
        "stdev" => PivotFunc::StdDev,
        "stdevp" => PivotFunc::StdDevP,
        "var" => PivotFunc::Var,
        "varp" => PivotFunc::VarP,
        _ => PivotFunc::Sum,
    }
}

fn func_name(f: PivotFunc) -> &'static str {
    match f {
        PivotFunc::Sum => "sum",
        PivotFunc::Count => "count",
        PivotFunc::Average => "average",
        PivotFunc::Max => "max",
        PivotFunc::Min => "min",
        PivotFunc::Product => "product",
        PivotFunc::CountNumbers => "countnums",
        PivotFunc::StdDev => "stdev",
        PivotFunc::StdDevP => "stdevp",
        PivotFunc::Var => "var",
        PivotFunc::VarP => "varp",
    }
}

/// The function's name as a value field's caption (`Sum - Sales`, LibreOffice's spelling).
fn caption(f: PivotFunc, col: &str) -> String {
    let n = match f {
        PivotFunc::Sum => "Sum",
        PivotFunc::Count | PivotFunc::CountNumbers => "Count",
        PivotFunc::Average => "Mean",
        PivotFunc::Max => "Max",
        PivotFunc::Min => "Min",
        PivotFunc::Product => "Product",
        PivotFunc::StdDev => "StDev",
        PivotFunc::StdDevP => "StDevP",
        PivotFunc::Var => "Var",
        PivotFunc::VarP => "VarP",
    };
    format!("{n} - {col}")
}

fn level(f: &Elem) -> Option<&Elem> {
    f.child(Ns::Table, "data-pilot-level")
}

fn field(f: &Elem) -> PivotField {
    let name = f.attr(Ns::Table, "source-field-name").unwrap_or_default().to_string();
    let sort = match level(f).and_then(|l| l.child(Ns::Table, "data-pilot-sort-info")) {
        Some(s) if s.attr(Ns::Table, "sort-mode") == Some("none") || s.attr(Ns::Table, "sort-mode") == Some("manual") => PivotSort::None,
        Some(s) if s.attr(Ns::Table, "order") == Some("descending") => PivotSort::Desc,
        _ => PivotSort::Asc,
    };
    // A date group (`Years`, `Months`) is a field of its own over the source field.
    let groups = f.child(Ns::Table, "data-pilot-groups");
    let name = groups.and_then(|g| g.attr(Ns::Table, "source-field-name")).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or(name);
    let date_group = match groups.and_then(|g| g.attr(Ns::Table, "grouped-by")) {
        Some("years") => PivotDateGroup::Years,
        Some("quarters") => PivotDateGroup::Quarters,
        Some("months") => PivotDateGroup::Months,
        Some("days") => PivotDateGroup::Days,
        _ => PivotDateGroup::None,
    };
    let collapsed_items = level(f)
        .and_then(|l| l.child(Ns::Table, "data-pilot-members"))
        .map(|m| {
            m.children(Ns::Table, "data-pilot-member")
                .filter(|m| m.attr(Ns::Table, "show-details") == Some("false"))
                .filter_map(|m| m.attr(Ns::Table, "name").map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    PivotField { source_col: name, sort, date_group, collapsed_items }
}

/// A data-pilot table as a PivotTable, with the name of the sheet its report is on.
pub(crate) fn read(p: &Elem, id: u32) -> Option<(String, PivotTable)> {
    let (sheet, target) = formula::range_on_sheet(p.attr(Ns::Table, "target-range-address")?)?;
    let source = p.child(Ns::Table, "source-cell-range").and_then(|s| s.attr(Ns::Table, "cell-range-address")).and_then(formula::range_on_sheet)?;
    let src = gridcraft_formula::printer::reference_a1(&formula::absolute(&source.0, source.1, false));
    // ODF's layout, where no field states one, is tabular.
    let mut pt = PivotTable {
        id,
        name: p.attr(Ns::Table, "name").unwrap_or("DataPilot1").to_string(),
        source: src,
        layout: PivotLayout::Tabular,
        subtotals_top: false,
        ..PivotTable::default()
    };
    let mut layout_seen = false;
    for f in p.children(Ns::Table, "data-pilot-field") {
        let orientation = f.attr(Ns::Table, "orientation").unwrap_or("hidden");
        if f.attr(Ns::Table, "is-data-layout-field") == Some("true") {
            pt.values_on_rows = orientation == "row";
            if pt.values_on_rows {
                pt.values_position = Some(pt.rows.len());
            }
            continue;
        }
        match orientation {
            "row" => pt.rows.push(field(f)),
            "column" => pt.columns.push(field(f)),
            "page" => {
                let selected = f.attr(Ns::Table, "selected-page").map(|s| vec![s.to_string()]).or_else(|| {
                    let members: Vec<&Elem> = level(f)
                        .and_then(|l| l.child(Ns::Table, "data-pilot-members"))
                        .map(|m| m.children(Ns::Table, "data-pilot-member").collect())
                        .unwrap_or_default();
                    let hidden = members.iter().any(|m| m.attr(Ns::Table, "display") == Some("false"));
                    hidden.then(|| {
                        members
                            .iter()
                            .filter(|m| m.attr(Ns::Table, "display") != Some("false"))
                            .filter_map(|m| m.attr(Ns::Table, "name").map(str::to_string))
                            .collect()
                    })
                });
                pt.filters.push(PivotFilter { source_col: f.attr(Ns::Table, "source-field-name").unwrap_or_default().to_string(), selected });
            }
            "data" => {
                let col = f.attr(Ns::Table, "source-field-name").unwrap_or_default().to_string();
                let fun = func(f.attr(Ns::Table, "function").unwrap_or("sum"));
                let show_as = match f.child(Ns::Table, "data-pilot-field-reference").and_then(|r| r.attr(Ns::Table, "type")) {
                    Some("total-percentage") => PivotShowAs::PercentOfGrandTotal,
                    Some("column-percentage") => PivotShowAs::PercentOfColumnTotal,
                    Some("row-percentage") => PivotShowAs::PercentOfRowTotal,
                    Some("running-total") => PivotShowAs::RunningTotal,
                    // `index` is Excel's Index calculation, not a rank: there is no model for it.
                    _ => PivotShowAs::Normal,
                };
                let name = caption(fun, &col);
                pt.values.push(PivotValue { source_col: col, func: fun, name, show_as, number_format: None });
            }
            _ => {}
        }
        if !layout_seen && let Some(l) = level(f).and_then(|l| l.child(Ns::Table, "data-pilot-layout-info")) {
            layout_seen = true;
            pt.layout = match l.attr(Ns::Table, "layout-mode") {
                Some("outline-subtotals-top" | "outline-subtotals-bottom") => PivotLayout::Outline,
                Some("compact-layout") => PivotLayout::Compact,
                _ => PivotLayout::Tabular,
            };
            // Compact shows subtotals on top, as its group's own row; tabular never does.
            pt.subtotals_top = matches!(l.attr(Ns::Table, "layout-mode"), Some("outline-subtotals-top" | "compact-layout"));
        }
    }
    match p.attr(Ns::Table, "grand-total") {
        Some("none") => {
            pt.grand_totals_rows = false;
            pt.grand_totals_cols = false;
        }
        Some("row") => pt.grand_totals_cols = false,
        Some("column") => pt.grand_totals_rows = false,
        _ => {}
    }
    pt.filter_button = p.attr(Ns::Table, "show-filter-button") != Some("false");
    pt.anchor = CellRef::new(target.start.row.saturating_add(header_rows(pt.filter_button, shown_pages(&pt))).min(target.end.row), target.start.col);
    pt.last_range = Some(target);
    Some((sheet, pt))
}

/// A row or column field; a row field states the report's `layout` (and whether subtotals go on
/// top), as each of ODF's row levels does.
fn field_xml(o: &mut String, f: &PivotField, orientation: &str, layout: Option<(PivotLayout, bool)>) {
    let _ = write!(
        o,
        r#"<table:data-pilot-field table:source-field-name="{}" table:orientation="{orientation}" table:function="auto">"#,
        esc_attr(&f.source_col)
    );
    o.push_str(r#"<table:data-pilot-level table:show-empty="false">"#);
    if !f.collapsed_items.is_empty() {
        o.push_str("<table:data-pilot-members>");
        for m in &f.collapsed_items {
            let _ = write!(o, r#"<table:data-pilot-member table:name="{}" table:display="true" table:show-details="false"/>"#, esc_attr(m));
        }
        o.push_str("</table:data-pilot-members>");
    }
    let (mode, order) = match f.sort {
        PivotSort::Asc => ("name", "ascending"),
        PivotSort::Desc => ("name", "descending"),
        PivotSort::None => ("none", "ascending"),
    };
    let _ = write!(o, r#"<table:data-pilot-sort-info table:order="{order}" table:sort-mode="{mode}"/>"#);
    if let Some((layout, top)) = layout {
        let mode = match layout {
            PivotLayout::Tabular => "tabular-layout",
            PivotLayout::Compact => "compact-layout",
            PivotLayout::Outline if top => "outline-subtotals-top",
            PivotLayout::Outline => "outline-subtotals-bottom",
        };
        let _ = write!(o, r#"<table:data-pilot-layout-info table:layout-mode="{mode}" table:add-empty-lines="false"/>"#);
    }
    o.push_str("</table:data-pilot-level>");
    // Date grouping follows the level.
    if f.date_group != PivotDateGroup::None {
        let by = match f.date_group {
            PivotDateGroup::Years => "years",
            PivotDateGroup::Quarters => "quarters",
            PivotDateGroup::Months => "months",
            _ => "days",
        };
        let _ = write!(
            o,
            r#"<table:data-pilot-groups table:source-field-name="{}" table:date-start="auto" table:date-end="auto" table:grouped-by="{by}"/>"#,
            esc_attr(&f.source_col)
        );
    }
    o.push_str("</table:data-pilot-field>");
}

/// The most members listed for a page field.
const MAX_MEMBERS: usize = 10_000;

/// The distinct items of source column `col`, as the report labels them (its number format or
/// `group`ing, `(blank)` for an empty cell), in source order.
fn members(wb: &Workbook, sheet: usize, src: RangeRef, col: u32, group: PivotDateGroup) -> Vec<String> {
    let Some(sh) = wb.sheets.get(sheet) else { return Vec::new() };
    let labels = gridcraft_model::pivot_source::ItemLabels::new(wb, sheet, src, col, group);
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in src.start.row.saturating_add(1)..=src.end.row {
        // A record with nothing in it is no record.
        if (src.start.col..=src.end.col).all(|c| sh.value(CellRef::new(row, c)).is_empty()) {
            continue;
        }
        let label = labels.label(&sh.value(CellRef::new(row, col)));
        if seen.insert(label.to_lowercase()) {
            out.push(label);
            if out.len() >= MAX_MEMBERS {
                break;
            }
        }
    }
    out
}

/// The page fields shown above a report's body: one that is also a row or column field is not.
fn shown_pages(pt: &PivotTable) -> u32 {
    pt.filters.iter().filter(|f| pt.axis_field(&f.source_col).is_none()).count().min(u32::MAX as usize) as u32
}

/// The rows of a report's target range above its body: LibreOffice's Filter button row, the
/// `pages` shown, and a blank row after them. The writer starts the target this far above the
/// body, so the anchor (the body's top-left) reads back.
fn header_rows(button: bool, pages: u32) -> u32 {
    let above = u32::from(button).saturating_add(pages);
    above.saturating_add(u32::from(above > 0))
}

/// `table:data-pilot-tables` for every sheet's PivotTables (empty when there are none).
pub(crate) fn write(wb: &Workbook) -> String {
    let mut o = String::new();
    for (sheet_index, sh) in wb.sheets.iter().enumerate() {
        for pt in &sh.pivots {
            // The source as the report reads it, and its column names.
            let Ok((src_index, src_range)) = gridcraft_model::pivot_source::resolve(wb, &pt.source, sheet_index) else { continue };
            let Some(src_sheet) = wb.sheets.get(src_index).map(|s| s.name.as_str()) else { continue };
            let headers = gridcraft_model::pivot_source::headers(wb, src_index, src_range).unwrap_or_default();
            let column =
                |name: &str| headers.iter().position(|h| h.eq_ignore_ascii_case(name)).and_then(|i| src_range.start.col.checked_add(i as u32));
            // A page field on no column of the source is drawn nowhere (the report leaves it
            // out), so it isn't written.
            let mut pt = pt.clone();
            pt.filters.retain(|f| column(&f.source_col).is_some());
            let pt = &pt;
            // The body is where the report draws it: below its page fields, or at the anchor
            // when they fit above it. The target starts the header rows above that, the Filter
            // button row only where there is room for it.
            let pages = shown_pages(pt);
            let drawn = header_rows(false, pages);
            let body = if pt.anchor.row >= drawn { pt.anchor.row } else { pt.anchor.row.saturating_add(drawn) };
            let button = pt.filter_button && body >= header_rows(true, pages);
            let end = pt.last_range.map_or(pt.anchor, |r| r.end);
            let start = CellRef::new(body.saturating_sub(header_rows(button, pages)), pt.last_range.map_or(pt.anchor.col, |r| r.start.col));
            let target = RangeRef::new(start, CellRef::new(end.row.max(body), end.col.max(start.col)));
            let grand = match (pt.grand_totals_rows, pt.grand_totals_cols) {
                (true, true) => "both",
                (true, false) => "row",
                (false, true) => "column",
                (false, false) => "none",
            };
            let _ = write!(
                o,
                r#"<table:data-pilot-table table:name="{}" table:target-range-address="{}" table:grand-total="{grand}" table:show-filter-button="{button}"><table:source-cell-range table:cell-range-address="{}"/>"#,
                esc_attr(&pt.name),
                esc_attr(&formula::range_address(&sh.name, target)),
                esc_attr(&formula::range_address(src_sheet, src_range))
            );
            for f in &pt.filters {
                let selected = match f.selected.as_deref() {
                    Some([one]) => format!(" table:selected-page=\"{}\"", esc_attr(one)),
                    _ => String::new(),
                };
                let _ = write!(
                    o,
                    r#"<table:data-pilot-field table:source-field-name="{}" table:orientation="page" table:function="auto"{selected}><table:data-pilot-level table:show-empty="false">"#,
                    esc_attr(&f.source_col)
                );
                // Several items chosen: the others are listed as hidden members.
                if let Some(chosen) = f.selected.as_deref().filter(|s| s.len() > 1) {
                    let chosen: std::collections::HashSet<String> = chosen.iter().map(|c| c.to_lowercase()).collect();
                    o.push_str("<table:data-pilot-members>");
                    // A page field that is also a row or column field takes its grouping.
                    let group = pt.axis_field(&f.source_col).map_or(PivotDateGroup::None, |g| g.date_group);
                    for m in column(&f.source_col).map(|c| members(wb, src_index, src_range, c, group)).unwrap_or_default() {
                        let shown = chosen.contains(&m.to_lowercase());
                        let _ = write!(
                            o,
                            r#"<table:data-pilot-member table:name="{}" table:display="{shown}" table:show-details="true"/>"#,
                            esc_attr(&m)
                        );
                    }
                    o.push_str("</table:data-pilot-members>");
                }
                o.push_str("</table:data-pilot-level></table:data-pilot-field>");
            }
            // Σ Values (with two or more values) sits among the row fields where the report puts
            // it, or after the column fields.
            let layout = |o: &mut String, orientation: &str| {
                let _ = write!(
                    o,
                    r#"<table:data-pilot-field table:source-field-name="" table:is-data-layout-field="true" table:orientation="{orientation}" table:used-hierarchy="-1" table:function="auto"/>"#
                );
            };
            let layout_at = (pt.values.len() > 1 && pt.values_on_rows).then(|| pt.values_position.unwrap_or(pt.rows.len()).min(pt.rows.len()));
            for (i, f) in pt.rows.iter().enumerate() {
                if layout_at == Some(i) {
                    layout(&mut o, "row");
                }
                field_xml(&mut o, f, "row", Some((pt.layout, pt.subtotals_top)));
            }
            if layout_at == Some(pt.rows.len()) {
                layout(&mut o, "row");
            }
            for f in &pt.columns {
                field_xml(&mut o, f, "column", None);
            }
            if pt.values.len() > 1 && !pt.values_on_rows {
                layout(&mut o, "column");
            }
            for v in &pt.values {
                let _ = write!(
                    o,
                    r#"<table:data-pilot-field table:source-field-name="{}" table:orientation="data" table:function="{}">"#,
                    esc_attr(&v.source_col),
                    func_name(v.func)
                );
                let reference = match v.show_as {
                    PivotShowAs::Normal => None,
                    PivotShowAs::PercentOfGrandTotal => Some("total-percentage"),
                    PivotShowAs::PercentOfColumnTotal => Some("column-percentage"),
                    PivotShowAs::PercentOfRowTotal => Some("row-percentage"),
                    PivotShowAs::RunningTotal => Some("running-total"),
                    // ODF has no rank: the value is written plain.
                    PivotShowAs::Rank => None,
                };
                // A running total runs along the first row field (else the first column field).
                let base = match v.show_as {
                    PivotShowAs::RunningTotal => pt.rows.first().or(pt.columns.first()).map_or(&v.source_col, |f| &f.source_col),
                    _ => &v.source_col,
                };
                if let Some(t) = reference {
                    let _ = write!(o, r#"<table:data-pilot-field-reference table:field-name="{}" table:type="{t}"/>"#, esc_attr(base));
                }
                o.push_str("</table:data-pilot-field>");
            }
            o.push_str("</table:data-pilot-table>");
        }
    }
    if o.is_empty() { o } else { format!("<table:data-pilot-tables>{o}</table:data-pilot-tables>") }
}
