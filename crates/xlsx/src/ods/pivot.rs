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
    let src = {
        let a = gridcraft_formula::Anchor { row: source.1.start.row, col: source.1.start.col, row_abs: true, col_abs: true };
        let b = gridcraft_formula::Anchor { row: source.1.end.row, col: source.1.end.col, row_abs: true, col_abs: true };
        gridcraft_formula::printer::reference_a1(&gridcraft_formula::Reference {
            sheet: gridcraft_formula::SheetSel::Named(source.0.clone()),
            kind: gridcraft_formula::RefKind::Range(a, b),
        })
    };
    let mut pt = PivotTable { id, name: p.attr(Ns::Table, "name").unwrap_or("DataPilot1").to_string(), source: src, ..PivotTable::default() };
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
            pt.subtotals_top = l.attr(Ns::Table, "layout-mode") == Some("outline-subtotals-top");
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
    // Above the body: LibreOffice's Filter button row, the page fields, and a blank row.
    pt.filter_button = p.attr(Ns::Table, "show-filter-button") != Some("false");
    let button = u32::from(pt.filter_button);
    let pages = pt.filters.len() as u32;
    let k = button + pages + u32::from(button + pages > 0);
    pt.anchor = CellRef::new(target.start.row.saturating_add(k).min(target.end.row), target.start.col);
    pt.last_range = Some(target);
    Some((sheet, pt))
}

fn field_xml(o: &mut String, f: &PivotField, orientation: &str) {
    let _ = write!(
        o,
        r#"<table:data-pilot-field table:source-field-name="{}" table:orientation="{orientation}" table:function="auto">"#,
        esc_attr(&f.source_col)
    );
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
    let _ = write!(
        o,
        r#"<table:data-pilot-sort-info table:order="{order}" table:sort-mode="{mode}"/></table:data-pilot-level></table:data-pilot-field>"#
    );
}

/// The most members listed for a page field.
const MAX_MEMBERS: usize = 10_000;

/// The distinct values of the source column headed `col`, as the report names its items.
fn members(wb: &Workbook, sheet: &str, src: RangeRef, col: &str) -> Vec<String> {
    let Some(sh) = wb.sheets.iter().find(|s| s.name == sheet) else { return Vec::new() };
    let Some(c) = (src.start.col..=src.end.col).find(|c| sh.value(CellRef::new(src.start.row, *c)).display() == col) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_, cell) in sh.cells.iter().filter(|(p, _)| p.col == c && p.row > src.start.row && p.row <= src.end.row) {
        let v = cell.value.display();
        if seen.insert(v.clone()) {
            out.push(v);
            if out.len() >= MAX_MEMBERS {
                break;
            }
        }
    }
    out
}

/// `table:data-pilot-tables` for every sheet's PivotTables (empty when there are none).
pub(crate) fn write(wb: &Workbook) -> String {
    let mut o = String::new();
    for sh in &wb.sheets {
        for pt in &sh.pivots {
            let Ok(gridcraft_formula::Expr::Ref(src)) = gridcraft_formula::parse(pt.source.trim_start_matches('=')) else { continue };
            let src_sheet = src.sheet_name().unwrap_or(&sh.name).to_string();
            let target = pt.last_range.unwrap_or(RangeRef::cell(pt.anchor));
            let grand = match (pt.grand_totals_rows, pt.grand_totals_cols) {
                (true, true) => "both",
                (true, false) => "row",
                (false, true) => "column",
                (false, false) => "none",
            };
            let button = pt.filter_button;
            let _ = write!(
                o,
                r#"<table:data-pilot-table table:name="{}" table:target-range-address="{}" table:grand-total="{grand}" table:show-filter-button="{button}"><table:source-cell-range table:cell-range-address="{}"/>"#,
                esc_attr(&pt.name),
                esc_attr(&formula::range_address(&sh.name, target)),
                esc_attr(&formula::range_address(&src_sheet, src.range()))
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
                    o.push_str("<table:data-pilot-members>");
                    for m in members(wb, &src_sheet, src.range(), &f.source_col) {
                        let shown = chosen.contains(&m);
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
            for f in &pt.rows {
                field_xml(&mut o, f, "row");
            }
            for f in &pt.columns {
                field_xml(&mut o, f, "column");
            }
            if pt.values.len() > 1 {
                let _ = write!(
                    o,
                    r#"<table:data-pilot-field table:source-field-name="" table:is-data-layout-field="true" table:orientation="{}" table:used-hierarchy="-1" table:function="auto"/>"#,
                    if pt.values_on_rows { "row" } else { "column" }
                );
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
                if let Some(t) = reference {
                    let _ = write!(o, r#"<table:data-pilot-field-reference table:field-name="{}" table:type="{t}"/>"#, esc_attr(&v.source_col));
                }
                o.push_str("</table:data-pilot-field>");
            }
            o.push_str("</table:data-pilot-table>");
        }
    }
    if o.is_empty() { o } else { format!("<table:data-pilot-tables>{o}</table:data-pilot-tables>") }
}
