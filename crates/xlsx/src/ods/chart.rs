//! Charts: embedded `office:chart` objects (`Object N/content.xml`, ODF 1.3 part 3 §11) ↔
//! GridCraft charts — type, title, axis titles, legend, gridlines, data labels and series with
//! their names, categories, values, colours, secondary axis and smoothing.

use std::collections::HashMap;
use std::fmt::Write as _;

use gridcraft_model::{Chart, ChartKind, LegendPos, Series, Theme};

use super::dom::{Elem, Ns};
use super::formula;
use super::style::color;
use super::style::hex;
use crate::xml::{esc, esc_attr};

/// What a chart object says, before it is anchored on its sheet.
pub(crate) struct ChartDraft {
    pub kind: ChartKind,
    pub title: Option<String>,
    pub series: Vec<Series>,
    pub legend: LegendPos,
    pub data_labels: bool,
    pub gridlines: bool,
    pub x_title: Option<String>,
    pub y_title: Option<String>,
}

/// An ODF range address list (`Data.A2:Data.B4`, or several separated by spaces) as absolute
/// A1 text (`Data!$A$2:$B$4`, a union in parentheses for several).
fn range_text(list: &str) -> Option<String> {
    let parts: Vec<String> = formula::split_addresses(list).into_iter().map(one_range).collect::<Option<_>>()?;
    match parts.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        _ => Some(format!("({})", parts.join(","))),
    }
}

/// One ODF range address as an absolute A1 reference.
fn one_range(addr: &str) -> Option<String> {
    let mut r = formula::parse_odf_ref(addr)?;
    // `Title.B1:Title.B1` names one cell.
    if let gridcraft_formula::RefKind::Range(a, b) = r.kind
        && a.row == b.row
        && a.col == b.col
    {
        r.kind = gridcraft_formula::RefKind::Cell(a);
    }
    let abs = |a: &mut gridcraft_formula::Anchor| {
        a.row_abs = true;
        a.col_abs = true;
    };
    match &mut r.kind {
        gridcraft_formula::RefKind::Cell(a) => abs(a),
        gridcraft_formula::RefKind::Range(a, b) => {
            abs(a);
            abs(b);
        }
        _ => {}
    }
    Some(gridcraft_formula::printer::reference_a1(&r))
}

fn styles_of(root: &Elem) -> HashMap<String, &Elem> {
    let mut out = HashMap::new();
    for list in root.children(Ns::Office, "automatic-styles").chain(root.children(Ns::Office, "styles")) {
        for s in list.children(Ns::Style, "style") {
            if let Some(n) = s.attr(Ns::Style, "name") {
                out.insert(n.to_string(), s);
            }
        }
    }
    out
}

fn chart_prop<'a>(styles: &'a HashMap<String, &Elem>, e: &Elem, name: &str) -> Option<&'a str> {
    let s = styles.get(e.attr(Ns::Chart, "style-name")?)?;
    s.child(Ns::Style, "chart-properties")?.attr(Ns::Chart, name)
}

fn graphic_prop<'a>(styles: &'a HashMap<String, &Elem>, e: &Elem, ns: Ns, name: &str) -> Option<&'a str> {
    let s = styles.get(e.attr(Ns::Chart, "style-name")?)?;
    s.child(Ns::Style, "graphic-properties")?.attr(ns, name)
}

fn title_text(e: Option<&Elem>) -> Option<String> {
    e.map(Elem::paragraphs_text).filter(|t| !t.is_empty())
}

/// The kind a chart class (`chart:bar`) and its properties name.
fn kind_of(class: &str, vertical: bool, stacked: bool, percent: bool, markers: bool, lines: bool) -> ChartKind {
    match class.trim_start_matches("chart:") {
        "bar" => match (vertical, stacked, percent) {
            (true, _, true) => ChartKind::BarStacked100,
            (true, true, _) => ChartKind::BarStacked,
            (true, ..) => ChartKind::BarClustered,
            (false, _, true) => ChartKind::ColumnStacked100,
            (false, true, _) => ChartKind::ColumnStacked,
            _ => ChartKind::ColumnClustered,
        },
        "line" if stacked || percent => ChartKind::LineStacked,
        "line" if markers => ChartKind::LineMarkers,
        "line" => ChartKind::Line,
        "area" if percent => ChartKind::AreaStacked100,
        "area" if stacked => ChartKind::AreaStacked,
        "area" => ChartKind::Area,
        "circle" => ChartKind::Pie,
        "ring" => ChartKind::Doughnut,
        "scatter" if lines => ChartKind::ScatterLines,
        "scatter" => ChartKind::Scatter,
        "bubble" => ChartKind::Bubble,
        "radar" | "filled-radar" => ChartKind::Radar,
        "stock" => ChartKind::Stock,
        _ => ChartKind::ColumnClustered,
    }
}

/// Reads an embedded chart object's `content.xml` tree.
pub(crate) fn read(root: &Elem) -> Option<ChartDraft> {
    let chart = root.child(Ns::Office, "body")?.child(Ns::Office, "chart")?.child(Ns::Chart, "chart")?;
    let styles = styles_of(root);
    let class = chart.attr(Ns::Chart, "class").unwrap_or("chart:bar");
    let plot = chart.child(Ns::Chart, "plot-area")?;
    let flag = |name: &str| chart_prop(&styles, plot, name) == Some("true");
    let (vertical, stacked, percent) = (flag("vertical"), flag("stacked"), flag("percentage"));
    let smooth = chart_prop(&styles, plot, "interpolation").is_some_and(|i| i != "none");
    let axes: Vec<&Elem> = plot.children(Ns::Chart, "axis").collect();
    let axis = |dim: &str| axes.iter().find(|a| a.attr(Ns::Chart, "dimension") == Some(dim)).copied();
    let categories =
        axis("x").and_then(|a| a.child(Ns::Chart, "categories")).and_then(|c| c.attr(Ns::Table, "cell-range-address")).and_then(range_text);
    let gridlines = axis("y").is_some_and(|a| a.children(Ns::Chart, "grid").any(|g| g.attr(Ns::Chart, "class") != Some("minor")));
    let mut data_labels = false;
    let mut series = Vec::new();
    let mut classes = Vec::new();
    let mut any_markers = false;
    let mut any_lines = false;
    for s in plot.children(Ns::Chart, "series") {
        let Some(values) = s.attr(Ns::Chart, "values-cell-range-address").and_then(range_text) else { continue };
        let own_class = s.attr(Ns::Chart, "class").unwrap_or(class);
        classes.push(own_class.to_string());
        let symbol = chart_prop(&styles, s, "symbol-type").or_else(|| chart_prop(&styles, plot, "symbol-type"));
        let markers = symbol.is_some_and(|t| t != "none");
        let stroke = graphic_prop(&styles, s, Ns::Draw, "stroke");
        let lines = stroke != Some("none");
        any_markers |= markers;
        any_lines |= lines;
        if chart_prop(&styles, s, "data-label-number").is_some_and(|v| v != "none") || chart_prop(&styles, s, "data-label-text") == Some("true") {
            data_labels = true;
        }
        let line_like = matches!(own_class.trim_start_matches("chart:"), "line" | "scatter" | "radar");
        let colour = if line_like { graphic_prop(&styles, s, Ns::Svg, "stroke-color") } else { graphic_prop(&styles, s, Ns::Draw, "fill-color") };
        // A scatter series' x values are its first domain.
        let domain = s.child(Ns::Chart, "domain").and_then(|d| d.attr(Ns::Table, "cell-range-address")).and_then(range_text);
        series.push(Series {
            name: s.attr(Ns::Chart, "label-cell-address").and_then(range_text),
            categories: domain.or_else(|| categories.clone()),
            values,
            color: colour.and_then(color),
            secondary: s.attr(Ns::Chart, "attached-axis").is_some_and(|a| a.contains("secondary")),
            smooth,
            ..Series::default()
        });
    }
    let base = kind_of(class, vertical, stacked, percent, any_markers, any_lines);
    // Series of another class than the chart's make it a combination chart.
    let combo = classes.iter().any(|c| c.as_str() != class);
    if combo {
        for (s, c) in series.iter_mut().zip(&classes) {
            s.kind = Some(kind_of(c, vertical, stacked, percent, any_markers, any_lines));
        }
    }
    let legend = match chart.child(Ns::Chart, "legend").and_then(|l| l.attr(Ns::Chart, "legend-position")) {
        None if chart.child(Ns::Chart, "legend").is_none() => LegendPos::None,
        Some("top" | "top-start" | "top-end") => LegendPos::Top,
        Some("start") => LegendPos::Left,
        Some("bottom" | "bottom-start" | "bottom-end") => LegendPos::Bottom,
        _ => LegendPos::Right,
    };
    Some(ChartDraft {
        kind: if combo { ChartKind::Combo } else { base },
        title: title_text(chart.child(Ns::Chart, "title")),
        series,
        legend,
        data_labels,
        gridlines,
        x_title: title_text(axis("x").and_then(|a| a.child(Ns::Chart, "title"))),
        y_title: title_text(axis("y").and_then(|a| a.child(Ns::Chart, "title"))),
    })
}

// ------------------------------------------------------------------------------------- writing

/// An A1 reference (`Sheet1!$B$2:$B$5`), or a union of them in parentheses, as an ODF range
/// address list (`$Sheet1.$B$2:.$B$5`, space-separated).
fn odf_range(a1: &str) -> Option<String> {
    let e = gridcraft_formula::parse(a1.trim_start_matches('=')).ok()?;
    let mut refs = Vec::new();
    let mut pending = vec![&e];
    while let Some(x) = pending.pop() {
        match x {
            gridcraft_formula::Expr::Ref(r) => refs.push(formula::address(r)),
            gridcraft_formula::Expr::Paren(inner) => pending.push(inner),
            gridcraft_formula::Expr::Binary(gridcraft_formula::BinOp::Union, a, b) => {
                pending.push(b);
                pending.push(a);
            }
            _ => return None,
        }
    }
    (!refs.is_empty()).then(|| refs.join(" "))
}

/// The class and chart properties of a kind: (class, vertical, stacked, percent, markers).
fn class_of(kind: ChartKind) -> (&'static str, bool, bool, bool, bool) {
    match kind {
        ChartKind::ColumnClustered | ChartKind::Histogram | ChartKind::Waterfall | ChartKind::Combo | ChartKind::BoxWhisker => {
            ("chart:bar", false, false, false, false)
        }
        ChartKind::ColumnStacked => ("chart:bar", false, true, false, false),
        ChartKind::ColumnStacked100 => ("chart:bar", false, true, true, false),
        ChartKind::BarClustered | ChartKind::Funnel => ("chart:bar", true, false, false, false),
        ChartKind::BarStacked => ("chart:bar", true, true, false, false),
        ChartKind::BarStacked100 => ("chart:bar", true, true, true, false),
        ChartKind::Line => ("chart:line", false, false, false, false),
        ChartKind::LineMarkers => ("chart:line", false, false, false, true),
        ChartKind::LineStacked => ("chart:line", false, true, false, false),
        ChartKind::Pie | ChartKind::Treemap | ChartKind::Sunburst => ("chart:circle", false, false, false, false),
        ChartKind::Doughnut => ("chart:ring", false, false, false, false),
        ChartKind::Area => ("chart:area", false, false, false, false),
        ChartKind::AreaStacked => ("chart:area", false, true, false, false),
        ChartKind::AreaStacked100 => ("chart:area", false, true, true, false),
        ChartKind::Scatter => ("chart:scatter", false, false, false, true),
        ChartKind::ScatterLines => ("chart:scatter", false, false, false, true),
        ChartKind::Bubble => ("chart:bubble", false, false, false, false),
        ChartKind::Radar => ("chart:radar", false, false, false, false),
        ChartKind::Stock => ("chart:stock", false, false, false, false),
    }
}

fn title_xml(o: &mut String, tag: &str, text: &Option<String>) {
    if let Some(t) = text.as_deref().filter(|t| !t.is_empty()) {
        let _ = write!(o, "<chart:{tag}>");
        for line in t.split('\n') {
            let _ = write!(o, "<text:p>{}</text:p>", esc(line));
        }
        let _ = write!(o, "</chart:{tag}>");
    }
}

/// A chart object's document, as its parts.
pub(crate) struct ChartDoc {
    /// The automatic styles' children.
    styles: String,
    /// `office:body`.
    body: String,
}

impl ChartDoc {
    /// The object's `content.xml` in a package.
    pub(crate) fn package(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:chart="urn:oasis:names:tc:opendocument:xmlns:chart:1.0" office:version="1.3"><office:automatic-styles>{}</office:automatic-styles>{}</office:document-content>"#,
            self.styles, self.body
        )
    }

    /// The object's document inline in a flat document, whose root declares the namespaces.
    pub(crate) fn inline(&self) -> String {
        format!(
            r#"<office:document office:mimetype="application/vnd.oasis.opendocument.chart" office:version="1.3"><office:automatic-styles>{}</office:automatic-styles>{}</office:document>"#,
            self.styles, self.body
        )
    }
}

/// The document of the chart object for `c`.
pub(crate) fn write(c: &Chart, theme: &Theme) -> ChartDoc {
    let (class, vertical, stacked, percent, markers) = class_of(c.kind);
    let mut styles = String::new();
    let _ = write!(
        styles,
        r#"<style:style style:name="chplot" style:family="chart"><style:chart-properties chart:vertical="{vertical}" chart:stacked="{stacked}" chart:percentage="{percent}" chart:symbol-type="{}" chart:interpolation="{}" chart:include-hidden-cells="false" chart:treat-empty-cells="leave-gap" chart:right-angled-axes="true"/></style:style>"#,
        if markers { "automatic" } else { "none" },
        if c.series.iter().any(|s| s.smooth) { "cubic-spline" } else { "none" }
    );
    styles
        .push_str(r##"<style:style style:name="chgrid" style:family="chart"><style:graphic-properties svg:stroke-color="#b3b3b3"/></style:style>"##);
    styles.push_str(r##"<style:style style:name="chaxis" style:family="chart"><style:chart-properties chart:display-label="true"/><style:graphic-properties svg:stroke-color="#b3b3b3"/></style:style>"##);
    let mut series = String::new();
    let mut cell_ranges: Vec<String> = Vec::new();
    for (i, s) in c.series.iter().enumerate() {
        let Some(values) = odf_range(&s.values) else { continue };
        cell_ranges.push(values.clone());
        let own = s.kind.map(class_of).map(|k| k.0).unwrap_or(class);
        let line_like = matches!(own, "chart:line" | "chart:scatter" | "chart:radar");
        let mut graphic = String::new();
        if let Some(col) = s.color.and_then(|c| hex(c, theme)) {
            let _ = if line_like {
                write!(graphic, " svg:stroke-color=\"{col}\" draw:fill-color=\"{col}\"")
            } else {
                write!(graphic, " draw:fill-color=\"{col}\"")
            };
        }
        if c.kind == ChartKind::Scatter {
            graphic.push_str(" draw:stroke=\"none\"");
        }
        let label = if c.data_labels { " chart:data-label-number=\"value\"" } else { "" };
        let _ = write!(
            styles,
            r#"<style:style style:name="chs{i}" style:family="chart"><style:chart-properties{label}/><style:graphic-properties{graphic}/></style:style>"#
        );
        let _ = write!(series, r#"<chart:series chart:style-name="chs{i}" chart:values-cell-range-address="{}""#, esc_attr(&values));
        if let Some(name) = s.name.as_deref().and_then(odf_range) {
            let _ = write!(series, " chart:label-cell-address=\"{}\"", esc_attr(&name));
        }
        if s.kind.is_some() {
            let _ = write!(series, " chart:class=\"{own}\"");
        }
        if s.secondary {
            series.push_str(" chart:attached-axis=\"secondary-y\"");
        }
        series.push('>');
        if matches!(c.kind, ChartKind::Scatter | ChartKind::ScatterLines | ChartKind::Bubble)
            && let Some(x) = s.categories.as_deref().and_then(odf_range)
        {
            let _ = write!(series, "<chart:domain table:cell-range-address=\"{}\"/>", esc_attr(&x));
        }
        series.push_str("</chart:series>");
    }
    let categories = c.series.first().and_then(|s| s.categories.as_deref()).and_then(odf_range);
    let mut o = format!(
        r#"<office:body><office:chart><chart:chart svg:width="{}pt" svg:height="{}pt" chart:class="{class}">"#,
        c.anchor.width * 0.75,
        c.anchor.height * 0.75
    );
    title_xml(&mut o, "title", &c.title);
    let legend = match c.legend {
        LegendPos::None => None,
        LegendPos::Bottom => Some("bottom"),
        LegendPos::Top => Some("top"),
        LegendPos::Left => Some("start"),
        LegendPos::Right => Some("end"),
    };
    if let Some(pos) = legend {
        let _ = write!(o, "<chart:legend chart:legend-position=\"{pos}\"/>");
    }
    let ranges = cell_ranges.iter().chain(categories.iter()).cloned().collect::<Vec<_>>().join(" ");
    let _ = write!(
        o,
        r#"<chart:plot-area chart:style-name="chplot" table:cell-range-address="{}" chart:data-source-has-labels="none">"#,
        esc_attr(&ranges)
    );
    if !matches!(c.kind, ChartKind::Pie | ChartKind::Doughnut) {
        o.push_str(r#"<chart:axis chart:dimension="x" chart:name="primary-x" chart:style-name="chaxis">"#);
        title_xml(&mut o, "title", &c.x_title);
        if let Some(cats) = categories.as_ref().filter(|_| !matches!(c.kind, ChartKind::Scatter | ChartKind::ScatterLines | ChartKind::Bubble)) {
            let _ = write!(o, "<chart:categories table:cell-range-address=\"{}\"/>", esc_attr(cats));
        }
        o.push_str("</chart:axis>");
        o.push_str(r#"<chart:axis chart:dimension="y" chart:name="primary-y" chart:style-name="chaxis">"#);
        title_xml(&mut o, "title", &c.y_title);
        if c.gridlines {
            o.push_str(r#"<chart:grid chart:style-name="chgrid" chart:class="major"/>"#);
        }
        o.push_str("</chart:axis>");
        if c.series.iter().any(|s| s.secondary) {
            o.push_str(r#"<chart:axis chart:dimension="y" chart:name="secondary-y" chart:style-name="chaxis"/>"#);
        }
    } else if let Some(cats) = &categories {
        let _ = write!(
            o,
            r#"<chart:axis chart:dimension="x" chart:name="primary-x"><chart:categories table:cell-range-address="{}"/></chart:axis>"#,
            esc_attr(cats)
        );
    }
    o.push_str(&series);
    o.push_str("</chart:plot-area></chart:chart></office:chart></office:body>");
    ChartDoc { styles, body: o }
}

#[cfg(test)]
mod tests {
    use super::super::dom::parse_tree;
    use super::*;

    #[test]
    fn a_chart_survives_writing_and_reading() {
        let chart = Chart {
            id: 1,
            kind: ChartKind::BarStacked,
            anchor: gridcraft_model::Anchor { width: 300.0, height: 200.0, ..Default::default() },
            title: Some("Sales".into()),
            series: vec![Series {
                name: Some("Data!$B$1".into()),
                categories: Some("Data!$A$2:$A$4".into()),
                values: "Data!$B$2:$B$4".into(),
                color: Some(gridcraft_model::Color::Rgb(0x004586)),
                ..Series::default()
            }],
            legend: LegendPos::Right,
            data_labels: true,
            data_table: None,
            gridlines: true,
            style: 0,
            x_title: Some("Month".into()),
            y_title: None,
            source: None,
            by_rows: false,
            pivot: None,
        };
        let xml = write(&chart, &Theme::default()).package();
        let tree = parse_tree(xml.as_bytes()).unwrap();
        let d = read(&tree).unwrap();
        assert_eq!(d.kind, ChartKind::BarStacked);
        assert_eq!(d.title.as_deref(), Some("Sales"));
        assert_eq!(d.x_title.as_deref(), Some("Month"));
        assert_eq!(d.legend, LegendPos::Right);
        assert!(d.gridlines && d.data_labels);
        assert_eq!(d.series.len(), 1);
        assert_eq!(d.series[0].values, "Data!$B$2:$B$4");
        assert_eq!(d.series[0].categories.as_deref(), Some("Data!$A$2:$A$4"));
        assert_eq!(d.series[0].name.as_deref(), Some("Data!$B$1"));
        assert_eq!(d.series[0].color, Some(gridcraft_model::Color::Rgb(0x004586)));
    }

    /// A series over several ranges reads as their union, and writes back as the list.
    #[test]
    fn range_lists_are_unions() {
        assert_eq!(range_text("Data.A1:Data.A3 Data.C1:Data.C3").as_deref(), Some("(Data!$A$1:$A$3,Data!$C$1:$C$3)"));
        assert_eq!(range_text("Data.B2").as_deref(), Some("Data!$B$2"));
        assert_eq!(odf_range("(Data!$A$1:$A$3,Data!$C$1:$C$3)").as_deref(), Some("$Data.$A$1:.$A$3 $Data.$C$1:.$C$3"));
        assert_eq!(range_text(&odf_range("(Data!$A$1:$A$3,Data!$C$1:$C$3)").unwrap_or_default()).as_deref(), Some("(Data!$A$1:$A$3,Data!$C$1:$C$3)"));
    }
}
