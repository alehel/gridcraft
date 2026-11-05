//! Importing a combination chart keeps each series' own group kind, the first group's included.

use gridcraft_model::ChartKind;

use crate::chart::read_chart;
use crate::xml::parse;

fn ser(i: usize) -> String {
    format!("<c:ser><c:idx val=\"{i}\"/><c:order val=\"{i}\"/><c:val><c:numRef><c:f>Data!$A$1:$A$5</c:f></c:numRef></c:val></c:ser>")
}

const AX: &str = "<c:axId val=\"1\"/><c:axId val=\"2\"/>";

fn bar(sers: &str) -> String {
    format!("<c:barChart><c:barDir val=\"col\"/><c:grouping val=\"clustered\"/>{sers}{AX}</c:barChart>")
}

fn line(sers: &str) -> String {
    format!("<c:lineChart><c:grouping val=\"standard\"/>{sers}{AX}</c:lineChart>")
}

fn kinds(first: &str, second: &str) -> Vec<Option<ChartKind>> {
    let xml = format!(
        "<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\">\
         <c:chart><c:plotArea>{first}{second}</c:plotArea></c:chart></c:chartSpace>"
    );
    let root = parse(xml.as_bytes()).unwrap();
    let ch = read_chart(&root).unwrap();
    assert_eq!(ch.kind, ChartKind::Combo);
    ch.series.iter().map(|s| s.kind).collect()
}

#[test]
fn first_group_with_two_columns_stays_columns() {
    let k = kinds(&bar(&(ser(0) + &ser(1))), &line(&ser(2)));
    assert_eq!(k[0], Some(ChartKind::ColumnClustered));
    assert_eq!(k[1], Some(ChartKind::ColumnClustered));
    assert!(matches!(k[2], Some(ChartKind::Line | ChartKind::LineMarkers)));
}

#[test]
fn line_first_group_stays_a_line() {
    let k = kinds(&line(&ser(0)), &bar(&ser(1)));
    assert!(matches!(k[0], Some(ChartKind::Line | ChartKind::LineMarkers)));
    assert_eq!(k[1], Some(ChartKind::ColumnClustered));
}

/// Outside a combo, a series keeps its own markers or lines where they differ from its chart's,
/// read and written. The chart's kind is its first series'.
#[test]
fn series_keep_their_own_markers_and_lines() {
    use gridcraft_model::{Chart, Series, Workbook};
    let no_marker = "<c:ser><c:idx val=\"0\"/><c:order val=\"0\"/><c:marker><c:symbol val=\"none\"/></c:marker><c:val><c:numRef><c:f>Data!$A$1:$A$5</c:f></c:numRef></c:val></c:ser>";
    let xml = format!(
        "<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"><c:chart><c:plotArea>{}</c:plotArea></c:chart></c:chartSpace>",
        line(&(no_marker.to_string() + &ser(1)))
    );
    let ch = read_chart(&parse(xml.as_bytes()).unwrap()).unwrap();
    assert_eq!((ch.kind, ch.series.iter().map(|s| s.kind).collect::<Vec<_>>()), (ChartKind::Line, vec![None, Some(ChartKind::LineMarkers)]));
    let series = |kind| Series { values: "Sheet1!$A$1:$A$3".into(), kind, ..Series::default() };
    for (kind, kinds) in
        [(ChartKind::Line, vec![None, Some(ChartKind::LineMarkers)]), (ChartKind::ScatterLines, vec![Some(ChartKind::Scatter), None])]
    {
        let chart = Chart { kind, series: kinds.iter().map(|k| series(*k)).collect(), ..ch.clone() };
        let written = crate::chart::write_chart(&Workbook::new(), 0, &chart, "Book1");
        let back = read_chart(&parse(written.as_bytes()).unwrap()).unwrap();
        // XLSX has no chart-wide kind besides its series': each series is drawn as it was.
        let drawn = |c: &Chart| (0..c.series.len()).map(|i| c.series_kind(i)).collect::<Vec<_>>();
        assert_eq!(drawn(&back), drawn(&chart), "{kind:?}");
    }
}

/// A combo chart is written with one group per kind its series are drawn as, and read back in
/// the series' own order.
#[test]
fn combo_series_keep_their_kinds_and_order() {
    use gridcraft_model::{Chart, Series, Workbook};
    let xml = format!(
        "<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"><c:chart><c:plotArea>{}{}</c:plotArea></c:chart></c:chartSpace>",
        bar(&ser(0)),
        line(&ser(1))
    );
    let base = read_chart(&parse(xml.as_bytes()).unwrap()).unwrap();
    for kinds in [
        vec![ChartKind::ColumnClustered, ChartKind::Area],
        vec![ChartKind::ColumnStacked, ChartKind::LineMarkers],
        vec![ChartKind::BarClustered, ChartKind::Line],
        vec![ChartKind::LineMarkers, ChartKind::ColumnClustered],
    ] {
        let series: Vec<Series> = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| Series { values: format!("Sheet1!${}$1:${}$3", ["B", "C"][i], ["B", "C"][i]), kind: Some(*k), ..Series::default() })
            .collect();
        let chart = Chart { kind: ChartKind::Combo, series, ..base.clone() };
        let back = read_chart(&parse(crate::chart::write_chart(&Workbook::new(), 0, &chart, "Book1").as_bytes()).unwrap()).unwrap();
        let drawn = |c: &Chart| (0..c.series.len()).map(|i| (c.series_kind(i), c.series[i].values.clone())).collect::<Vec<_>>();
        assert_eq!(drawn(&back), drawn(&chart), "{kinds:?}");
    }
}
