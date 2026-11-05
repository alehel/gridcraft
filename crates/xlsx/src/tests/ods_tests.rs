//! Original ODS packages assembled in Rust, not files from another spreadsheet application.

use gridcraft_core::{CellRef, RangeRef, Value};

use super::make_zip;
use crate::{Format, IoError, read_ods, read_xlsx, sniff, write_ods, write_xlsx};

const MIME: &[u8] = b"application/vnd.oasis.opendocument.spreadsheet";
const MANIFEST: &[u8] = br#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#;

fn content(tables: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2"><office:body><office:spreadsheet>{tables}</office:spreadsheet></office:body></office:document-content>"#
    )
}
fn ods(tables: &str) -> Vec<u8> {
    make_zip(&[("mimetype", MIME), ("META-INF/manifest.xml", MANIFEST), ("content.xml", content(tables).as_bytes())])
}
fn row(cells: &str) -> Vec<u8> {
    ods(&format!(r#"<table:table table:name="Data"><table:table-row>{cells}</table:table-row></table:table>"#))
}
fn text(s: &str) -> Value {
    Value::Text(s.into())
}

/// A start tag with thousands of `xmlns:` declarations is refused instead of allocated
/// (RUSTSEC-2026-0195: the resolver kept a binding per declaration, with no cap).
#[test]
fn ods_namespace_declaration_bomb_is_rejected() {
    let bomb = |n: usize| -> String { (0..n).map(|i| format!(" xmlns:p{i}=\"urn:p{i}\"")).collect() };
    let doc = |decls: &str| {
        format!(
            r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"{decls}><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="float" office:value="1"><text:p>1</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
        )
    };
    let build = |decls: &str| make_zip(&[("mimetype", MIME), ("META-INF/manifest.xml", MANIFEST), ("content.xml", doc(decls).as_bytes())]);
    let ok = read_ods(&build(&bomb(8)));
    assert!(ok.is_ok(), "a handful of namespaces still reads: {:?}", ok.err());
    assert!(read_ods(&build(&bomb(10_000))).is_err(), "a tag with thousands of declarations is refused");
}

#[test]
fn ods_typed_values_and_formulas_survive_xlsx() {
    let bytes = row(r#"
        <table:table-cell office:value-type="float" office:value="42.5"/>
        <table:table-cell office:value-type="boolean" office:boolean-value="true"/>
        <table:table-cell office:value-type="percentage" office:value="0.25"/>
        <table:table-cell office:value-type="currency" office:currency="USD" office:value="19.99"/>
        <table:table-cell office:value-type="string" office:string-value="literal =1+1"/>
        <table:table-cell table:formula="of:=[.A1]*2" office:value-type="float" office:value="85"><text:p>85</text:p></table:table-cell>
        <table:table-cell table:formula="of:=1/0" office:value-type="string"><text:p>#DIV/0!</text:p></table:table-cell>
        <table:table-cell table:formula="of:=UNKNOWN()"/>
        <table:table-cell office:value-type="float" office:value="NaN"><text:p>unavailable</text:p></table:table-cell>
        <table:table-cell office:value-type="boolean" office:boolean-value="0"/>
    "#);
    let (wb, report) = read_ods(&bytes).unwrap();
    assert!(report.warnings.iter().any(|w| w.contains("unsupported or malformed")));
    let expected = [
        (Value::Number(42.5), None),
        (Value::Bool(true), None),
        (Value::Number(0.25), None),
        (Value::Number(19.99), None),
        (text("literal =1+1"), None),
        (Value::Number(85.0), Some("A1*2")),
        (Value::Error(gridcraft_core::CellError::Div0), Some("1/0")),
        (Value::Empty, Some("UNKNOWN()")),
        (text("unavailable"), None),
        (Value::Bool(false), None),
    ];
    let check = |wb: &gridcraft_model::Workbook| {
        for (c, (value, formula)) in expected.iter().enumerate() {
            let at = CellRef::new(0, c as u32);
            assert_eq!(&wb.sheets[0].value(at), value, "column {c}");
            assert_eq!(wb.sheets[0].cell(at).and_then(|c| c.formula.as_ref()).map(|f| f.text.as_str()), *formula, "column {c}");
        }
    };
    check(&wb);
    check(&read_xlsx(&write_xlsx(&wb).unwrap()).unwrap().0);
    check(&read_ods(&write_ods(&wb).unwrap()).unwrap().0);
}

#[test]
fn ods_ordered_text_preserves_paragraphs_runs_entities_spaces_tabs_and_breaks() {
    let bytes = row(
        r#"<table:table-cell office:value-type="string"><text:p>one <text:span>two &amp; three</text:span> four<text:s text:c="3"/><text:a>five</text:a><text:tab/>six<text:line-break/>seven&#33;<![CDATA[<eight>]]></text:p><text:p>nine</text:p><office:annotation><text:p>not cell text</text:p></office:annotation></table:table-cell>"#,
    );
    let (wb, _) = read_ods(&bytes).unwrap();
    assert_eq!(wb.sheets[0].value(CellRef::new(0, 0)), text("one two & three four   five\tsix\nseven!<eight>\nnine"));
    let bytes = row(r#"<table:table-cell office:value-type="string"><text:p>  one
        <text:span> two  </text:span>  three <text:s text:c="0"/>four<text:s text:c="2"/>  </text:p><text:p><text:s/>  five  </text:p></table:table-cell>"#);
    assert_eq!(read_ods(&bytes).unwrap().0.sheets[0].value(CellRef::new(0, 0)), text("one two three four  \n  five"));
}

#[test]
fn ods_repeats_merges_sheet_names_and_empty_tail_remain_sparse() {
    let bytes = ods(r#"
      <table:table table:name="First &amp; Second">
        <table:table-header-rows><table:table-row><table:table-cell table:number-columns-spanned="2" table:number-rows-spanned="2" office:value-type="string"><text:p>merged</text:p></table:table-cell><table:covered-table-cell office:value-type="float" office:value="99"/></table:table-row></table:table-header-rows>
        <table:table-row><table:covered-table-cell table:number-columns-repeated="2"/></table:table-row>
        <table:table-row-group><table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="3" office:value-type="float" office:value="7"/></table:table-row></table:table-row-group>
        <table:table-row table:number-rows-repeated="1048572"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>
      </table:table>
      <table:table table:name="Another"><table:table-row><table:table-cell office:value-type="string" office:string-value="second sheet"/></table:table-row></table:table>
    "#);
    let (wb, _) = read_ods(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 2);
    assert_eq!(wb.sheets[0].name, "First & Second");
    assert_eq!(wb.sheets[0].cells.len(), 8);
    assert_eq!(wb.sheets[0].value(CellRef::new(0, 1)), Value::Number(99.0));
    assert_eq!(wb.sheets[0].merges, vec![RangeRef::new(CellRef::new(0, 0), CellRef::new(1, 1))]);
    assert_eq!(wb.sheets[0].value(CellRef::new(3, 2)), Value::Number(7.0));
    assert_eq!(wb.sheets[1].value(CellRef::new(0, 0)), text("second sheet"));
}

#[test]
fn ods_dates_and_durations_use_formats_and_unsupported_values_stay_text() {
    let bytes = row(r#"
        <table:table-cell office:value-type="date" office:date-value="1900-02-28"/>
        <table:table-cell office:value-type="date" office:date-value="2026-10-07T18:30:00.125"/>
        <table:table-cell office:value-type="time" office:time-value="P1DT2H3M4.5S"/>
        <table:table-cell office:value-type="time" office:time-value="-PT2H"/>
        <table:table-cell office:value-type="date" office:date-value="1900-02-29"/>
        <table:table-cell office:value-type="date" office:date-value="2026-10-07T18:00:00Z"/>
        <table:table-cell office:value-type="time" office:time-value="P1M"/>
        <table:table-cell office:value-type="time" office:time-value="PT2M1H"/>
    "#);
    let (wb, report) = read_ods(&bytes).unwrap();
    let s = &wb.sheets[0];
    assert_eq!(s.value(CellRef::new(0, 0)), Value::Number(59.0));
    assert_eq!(s.value(CellRef::new(0, 1)), Value::Number(46302.0 + 66600.125 / 86400.0));
    assert_eq!(s.value(CellRef::new(0, 2)), Value::Number(1.0 + 7384.5 / 86400.0));
    assert_eq!(s.value(CellRef::new(0, 3)), Value::Number(-2.0 / 24.0));
    for (c, value) in [(4, "1900-02-29"), (5, "2026-10-07T18:00:00Z"), (6, "P1M"), (7, "PT2M1H")] {
        assert_eq!(s.value(CellRef::new(0, c)), text(value));
    }
    assert_eq!(wb.styles.get(s.cell(CellRef::new(0, 0)).unwrap().style).num_fmt.0.as_ref(), "yyyy-mm-dd");
    assert_eq!(wb.styles.get(s.cell(CellRef::new(0, 1)).unwrap().style).num_fmt.0.as_ref(), "yyyy-mm-dd hh:mm:ss");
    assert_eq!(wb.styles.get(s.cell(CellRef::new(0, 2)).unwrap().style).num_fmt.0.as_ref(), "[h]:mm:ss");
    assert!(!report.warnings.is_empty());
}

#[test]
fn ods_namespaces_are_resolved_not_inferred_from_prefix_or_local_name() {
    let xml = content(r#"<table:table table:name="Actual"><table:table-row><table:table-cell office:value-type="float" office:value="9"/></table:table-row></table:table>"#)
        .replace("<office:", "<o:").replace("</office:", "</o:").replace(" office:", " o:").replace("xmlns:office=", "xmlns:o=")
        .replace("<table:", "<t:").replace("</table:", "</t:").replace(" table:", " t:").replace("xmlns:table=", "xmlns:t=");
    let bytes = make_zip(&[("mimetype", MIME), ("content.xml", xml.as_bytes())]);
    assert_eq!(read_ods(&bytes).unwrap().0.sheets[0].value(CellRef::new(0, 0)), Value::Number(9.0));
    let spoofed = xml.replace("urn:oasis:names:tc:opendocument:xmlns:table:1.0", "wrong-namespace");
    assert!(read_ods(&make_zip(&[("mimetype", MIME), ("content.xml", spoofed.as_bytes())])).is_err());
    let bytes =
        row(r#"<table:table-cell xmlns:fake="wrong-namespace" fake:value-type="float" fake:value="123"><text:p>kept</text:p></table:table-cell>"#);
    assert_eq!(read_ods(&bytes).unwrap().0.sheets[0].value(CellRef::new(0, 0)), text("kept"));
}

#[test]
fn ods_rejects_bad_packages_encryption_and_truncated_xml() {
    assert!(read_ods(b"not zip").is_err());
    let xml = content(r#"<table:table table:name="Data"/>"#);
    for mime in [b"application/zip".as_slice(), b"application/vnd.oasis.opendocument.spreadsheet "] {
        assert!(read_ods(&make_zip(&[("mimetype", mime), ("content.xml", xml.as_bytes())])).is_err());
    }
    let encrypted =
        br#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:encryption-data/></manifest:manifest>"#;
    assert!(
        read_ods(&make_zip(&[("mimetype", MIME), ("META-INF/manifest.xml", encrypted), ("content.xml", xml.as_bytes())]))
            .unwrap_err()
            .to_string()
            .contains("encrypted")
    );
    for bad in [
        xml.trim_end_matches("</office:document-content>").to_owned(),
        xml.replace("</office:spreadsheet>", "</office:wrong>"),
        "<root/>".to_owned(),
        format!("{xml}{xml}"),
    ] {
        assert!(read_ods(&make_zip(&[("mimetype", MIME), ("content.xml", bad.as_bytes())])).is_err(), "{bad}");
    }
    let deep = content(&format!("{}{}", "<x>".repeat(300), "</x>".repeat(300)));
    assert!(read_ods(&make_zip(&[("mimetype", MIME), ("content.xml", deep.as_bytes())])).is_err());
}

#[test]
fn ods_expansion_bounds_stop_small_repeat_bombs() {
    for cells in [
        r#"<table:table-cell table:number-columns-repeated="4294967296"/>"#,
        r#"<table:table-cell table:number-columns-repeated="0"/>"#,
        r#"<table:table-cell table:number-columns-repeated="16384" table:number-columns-spanned="2" office:value-type="float" office:value="1"/>"#,
    ] {
        assert!(read_ods(&row(cells)).is_err(), "{cells}");
    }
    // Cells past the sheet's edge are clipped, as LibreOffice clips them, with a warning.
    for cells in [
        r#"<table:table-cell table:number-columns-repeated="4294967295" office:value-type="float" office:value="1"/>"#,
        r#"<table:table-cell table:number-columns-repeated="16384"/><table:table-cell office:value-type="float" office:value="1"/>"#,
    ] {
        let (wb, report) = read_ods(&row(cells)).unwrap();
        assert!(wb.sheets[0].cells.len() <= 16_384, "{cells}");
        assert!(report.warnings.iter().any(|w| w.contains("beyond column XFD")), "{cells}");
    }
    // Explicit spaces are capped with the cell's text.
    let (wb, _) =
        read_ods(&row(r#"<table:table-cell office:value-type="string"><text:p><text:s text:c="4294967295"/></text:p></table:table-cell>"#)).unwrap();
    assert!(matches!(wb.sheets[0].value(CellRef::new(0, 0)), Value::Text(t) if t.len() <= 1024 * 1024));
    // So is one long run of text in a single node.
    let long = format!(r#"<table:table-cell office:value-type="string"><text:p>{}</text:p></table:table-cell>"#, "é".repeat(800_000));
    let (wb, _) = read_ods(&row(&long)).unwrap();
    assert!(matches!(wb.sheets[0].value(CellRef::new(0, 0)), Value::Text(t) if t.len() <= 1024 * 1024 && t.len() > 1_000_000));
    // More than four million cells, or 256 MiB of text, are refused before they are stored.
    let cells = ods(
        r#"<table:table table:name="Data"><table:table-row table:number-rows-repeated="1048576"><table:table-cell table:number-columns-repeated="5" office:value-type="float" office:value="1"/></table:table-row></table:table>"#,
    );
    assert!(matches!(read_ods(&cells), Err(IoError::TooLarge(_))));
    let text = format!(
        r#"<table:table table:name="Data"><table:table-row table:number-rows-repeated="300000"><table:table-cell office:value-type="string" office:string-value="{}"/></table:table-row></table:table>"#,
        "x".repeat(1024)
    );
    assert!(matches!(read_ods(&ods(&text)), Err(IoError::TooLarge(_))));
    let cell = format!(r#"<table:table-cell office:value-type="string" office:string-value="{}"/>"#, "y".repeat(1024));
    let rows = |n: u32| {
        ods(&format!(r#"<table:table table:name="Data"><table:table-row table:number-rows-repeated="{n}">{cell}</table:table-row></table:table>"#))
    };
    assert!(read_ods(&rows(1000)).is_ok());
    assert!(matches!(read_ods(&rows(262_145)), Err(IoError::TooLarge(_))));
}

#[test]
fn ods_sniff_requires_exact_bounded_mimetype() {
    assert_eq!(sniff(&row("")), Format::Ods);
    assert_eq!(sniff(&make_zip(&[("mimetype", b"application/zip")])), Format::Xlsx);
    assert_eq!(sniff(&make_zip(&[("elsewhere", MIME)])), Format::Xlsx);
    assert_eq!(sniff(&make_zip(&[("mimetype", &vec![b'x'; 1_000_000])])), Format::Xlsx);
    assert_eq!(sniff(b"a,b\n1,2"), Format::Csv);
}

#[test]
fn ods_invalid_sheet_names_are_made_safe_for_xlsx() {
    let long = "A".repeat(50);
    let bytes = ods(&format!(
        r#"<table:table table:name="{long}"/><table:table table:name="{long}"/><table:table table:name="'History'"/><table:table table:name="[]:*?"/>"#
    ));
    let (wb, report) = read_ods(&bytes).unwrap();
    for (i, sheet) in wb.sheets.iter().enumerate() {
        assert!(wb.check_sheet_name(&sheet.name, Some(i)).is_ok());
    }
    assert!(report.warnings.iter().any(|w| w.contains("sheet names")));
    assert_eq!(read_xlsx(&write_xlsx(&wb).unwrap()).unwrap().0.sheets.len(), 4);
}

#[test]
fn ods_extension_error_cache_never_imports_its_placeholder_zero() {
    let bytes = row(
        r#"<table:table-cell xmlns:calcext="urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0" calcext:value-type="error" office:value-type="float" office:value="0" table:formula="of:=1/0"><text:p>#DIV/0!</text:p></table:table-cell>"#,
    );
    let (wb, _) = read_ods(&bytes).unwrap();
    assert_eq!(wb.sheets[0].value(CellRef::new(0, 0)), Value::Error(gridcraft_core::CellError::Div0));
    // LibreOffice's numbered errors read as the Excel error it saves them as.
    let bytes = row(
        r#"<table:table-cell xmlns:calcext="urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0" calcext:value-type="error" office:value-type="string" table:formula="of:=[.B1]/0"><text:p>Err:532</text:p></table:table-cell><table:table-cell table:formula="of:=(1" office:value-type="string"><text:p>Err:508</text:p></table:table-cell>"#,
    );
    let (wb, _) = read_ods(&bytes).unwrap();
    assert_eq!(wb.sheets[0].value(CellRef::new(0, 0)), Value::Error(gridcraft_core::CellError::Div0));
    assert_eq!(wb.sheets[0].value(CellRef::new(0, 1)), Value::Error(gridcraft_core::CellError::Value));
}

#[test]
fn ods_blank_repeats_past_the_sheet_edge_are_clipped() {
    // Writers with larger sheets pad to their own edge; blank padding past ours carries no data.
    let bytes = ods(r#"<table:table table:name="Data">
        <table:table-row><table:table-cell office:value-type="float" office:value="1"/><table:table-cell table:number-columns-repeated="4294967295"/><table:table-cell/></table:table-row>
        <table:table-row table:number-rows-repeated="16777215"><table:table-cell table:number-columns-repeated="16777216"/></table:table-row>
        <table:table-row table:number-rows-repeated="5"><table:table-cell/></table:table-row>
      </table:table>"#);
    let (wb, _) = read_ods(&bytes).unwrap();
    assert_eq!(wb.sheets[0].cells.len(), 1);
    assert_eq!(wb.sheets[0].value(CellRef::new(0, 0)), Value::Number(1.0));
    // Data after the edge is dropped as LibreOffice drops it, never silently: a warning says so.
    let late = ods(
        r#"<table:table table:name="Data"><table:table-row table:number-rows-repeated="1048576"><table:table-cell/></table:table-row><table:table-row><table:table-cell office:value-type="float" office:value="1"/></table:table-row></table:table>"#,
    );
    let (wb, report) = read_ods(&late).unwrap();
    assert!(wb.sheets[0].cells.is_empty());
    assert!(report.warnings.iter().any(|w| w.contains("beyond row 1,048,576")));
}

/// Print titles that reach past the sheet's content still close their header-rows group.
#[test]
fn ods_title_rows_past_the_content_write_well_formed_xml() {
    let mut wb = gridcraft_model::Workbook::new();
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    sh.set_value(CellRef::new(0, 0), Value::Number(1.0));
    sh.print.title_rows = Some((0, 5));
    sh.print.print_area = Some(RangeRef::new(CellRef::new(0, 0), CellRef::new(9, 3)));
    let bytes = write_ods(&wb).unwrap();
    let (back, _) = read_ods(&bytes).unwrap();
    assert_eq!(back.sheets[0].value(CellRef::new(0, 0)), Value::Number(1.0));
    assert_eq!(back.sheets[0].print.title_rows, Some((0, 5)));
    assert_eq!(back.sheets[0].print.print_area, Some(RangeRef::new(CellRef::new(0, 0), CellRef::new(9, 3))));
}

/// Row groups and the header block nest, whatever their edges, and the empty rest of the sheet
/// is in no group.
#[test]
fn ods_row_groups_and_title_rows_nest_and_the_tail_is_ungrouped() {
    let mut wb = gridcraft_model::Workbook::new();
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    for r in 0..4 {
        sh.set_value(CellRef::new(r, 0), Value::Number(f64::from(r)));
    }
    // Row 0 is grouped, the title rows 1-2 straddle the group's end, and the last row is grouped.
    for r in [0, 1, 3] {
        sh.rows.entry(r).or_default().outline = 1;
    }
    sh.print.title_rows = Some((1, 2));
    let bytes = write_ods(&wb).unwrap();
    let (back, _) = read_ods(&bytes).unwrap();
    let back = &back.sheets[0];
    assert_eq!(back.print.title_rows, Some((1, 2)));
    assert_eq!(back.rows.get(&0).map(|i| i.outline), Some(1));
    assert_eq!(back.rows.get(&3).map(|i| i.outline), Some(1));
    assert!(back.rows.get(&4).is_none_or(|i| i.outline == 0));
    assert!(back.rows.get(&1_000_000).is_none_or(|i| i.outline == 0));
    for r in 0..4 {
        assert_eq!(back.value(CellRef::new(r, 0)), Value::Number(f64::from(r)));
    }
}

/// A flat document (`.fods`) holds the whole workbook in one XML file and reads back the same.
#[test]
fn ods_flat_documents_round_trip() {
    let mut wb = gridcraft_model::Workbook::new();
    let bold = wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| {
        s.font.bold = true;
        s.num_fmt = gridcraft_model::NumFmt::new("0.00%");
    });
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    sh.set_cell(CellRef::new(0, 0), gridcraft_model::Cell::styled(Value::Number(0.25), bold));
    sh.set_cell(CellRef::new(1, 0), gridcraft_model::Cell::formula(gridcraft_model::Formula::new("=A1*2")));
    sh.freeze = Some((1, 0));
    sh.images.push(gridcraft_model::Image {
        id: 1,
        anchor: gridcraft_model::Anchor { cell: CellRef::new(2, 1), width: 40.0, height: 30.0, ..Default::default() },
        data: std::sync::Arc::new(b"\x89PNG\r\n\x1a\nnot really".to_vec()),
        mime: "image/png".into(),
        alt: String::new(),
    });
    wb.props.title = "Flat".into();
    let bytes = crate::write_fods(&wb, None).unwrap();
    assert!(crate::is_flat_ods(&bytes));
    assert!(!crate::is_flat_ods(b"a,b\n1,2"));
    let (back, _) = crate::read_fods(&bytes).unwrap();
    let s = &back.sheets[0];
    assert_eq!(s.value(CellRef::new(0, 0)), Value::Number(0.25));
    let st = back.styles.get(s.style_id(CellRef::new(0, 0)));
    assert!(st.font.bold);
    assert_eq!(st.num_fmt.as_str(), "0.00%");
    assert_eq!(s.cell(CellRef::new(1, 0)).and_then(|c| c.formula.as_ref()).map(|f| f.text.as_str()), Some("A1*2"));
    assert_eq!(s.freeze, Some((1, 0)));
    assert_eq!(s.images.len(), 1);
    assert_eq!(s.images[0].data.as_slice(), b"\x89PNG\r\n\x1a\nnot really");
    assert_eq!(back.props.title, "Flat");
}

/// A protected sheet's and structure's SHA-256 keys open as passwords that check, and are written
/// back; in XLSX, which has no form for them, the protection is kept without the password.
#[test]
fn ods_protection_keys_are_passwords() {
    // SHA-256 of "abc", base64.
    let key = "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
    let doc = format!(
        r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet table:structure-protected="true" table:protection-key="{key}" table:protection-key-digest-algorithm="http://www.w3.org/2000/09/xmldsig#sha256"><table:table table:name="Data" table:protected="true" table:protection-key="{key}" table:protection-key-digest-algorithm="http://www.w3.org/2000/09/xmldsig#sha256"><table:table-row><table:table-cell office:value-type="float" office:value="1"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let bytes = make_zip(&[("mimetype", MIME), ("META-INF/manifest.xml", MANIFEST), ("content.xml", doc.as_bytes())]);
    let check = |wb: &gridcraft_model::Workbook| {
        let p = wb.sheets[0].protection.as_ref().and_then(|p| p.password.clone()).unwrap();
        assert!(p.verify("abc").unwrap() && !p.verify("x").unwrap());
        assert!(wb.protected_structure && wb.structure_password.as_ref().unwrap().verify("abc").unwrap());
    };
    let (wb, _) = read_ods(&bytes).unwrap();
    check(&wb);
    check(&read_ods(&write_ods(&wb).unwrap()).unwrap().0);
    let (x, _) = read_xlsx(&write_xlsx(&wb).unwrap()).unwrap();
    assert!(x.sheets[0].protection.is_some());
}

/// Relative links are relative to the package in ODF and to the file's folder in GridCraft; a
/// link to a sheet alone goes to its first cell.
#[test]
fn ods_links_keep_their_targets() {
    let bytes = row(r##"
        <table:table-cell office:value-type="string"><text:p><text:a xlink:href="../other.ods#Sheet1.A1" xmlns:xlink="http://www.w3.org/1999/xlink">next door</text:a></text:p></table:table-cell>
        <table:table-cell office:value-type="string"><text:p><text:a xlink:href="https://example.com/a" xmlns:xlink="http://www.w3.org/1999/xlink">web</text:a></text:p></table:table-cell>
        <table:table-cell office:value-type="string"><text:p><text:a xlink:href="#Data" xmlns:xlink="http://www.w3.org/1999/xlink">sheet</text:a></text:p></table:table-cell>
        <table:table-cell office:value-type="string"><text:p><text:a xlink:href="#Data.B2" xmlns:xlink="http://www.w3.org/1999/xlink">cell</text:a></text:p></table:table-cell>
    "##);
    let (wb, _) = read_ods(&bytes).unwrap();
    let target = |wb: &gridcraft_model::Workbook, c| wb.sheets[0].hyperlinks.get(&CellRef::new(0, c)).map(|h| h.target.clone()).unwrap();
    for wb in [wb.clone(), read_ods(&write_ods(&wb).unwrap()).unwrap().0] {
        assert_eq!(target(&wb, 0), "other.ods#Sheet1.A1");
        assert_eq!(target(&wb, 1), "https://example.com/a");
        assert_eq!(target(&wb, 2), "Data!A1");
        assert_eq!(target(&wb, 3), "Data!B2");
    }
}

/// A styled ODS body: `styles` goes in `office:automatic-styles`.
fn styled_ods(styles: &str, tables: &str) -> Vec<u8> {
    let content = format!(
        r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:automatic-styles>{styles}</office:automatic-styles><office:body><office:spreadsheet>{tables}</office:spreadsheet></office:body></office:document-content>"#
    );
    make_zip(&[("mimetype", MIME), ("META-INF/manifest.xml", MANIFEST), ("content.xml", content.as_bytes())])
}

const FILLED: &str =
    r##"<style:style style:name="ce1" style:family="table-cell"><style:table-cell-properties fo:background-color="#ff0000"/></style:style>"##;

/// Covered cells show their merge's first cell's format, read as each covered cell is met.
#[test]
fn ods_covered_cells_take_the_merge_anchor_style() {
    let (wb, _) = read_ods(&styled_ods(
        FILLED,
        r#"<table:table table:name="Data"><table:table-row><table:table-cell table:style-name="ce1" table:number-columns-spanned="2" table:number-rows-spanned="2"/><table:covered-table-cell/></table:table-row><table:table-row><table:covered-table-cell/><table:covered-table-cell/></table:table-row></table:table>"#,
    ))
    .unwrap();
    let sheet = &wb.sheets[0];
    let anchor = sheet.style_id(CellRef::new(0, 0));
    assert_ne!(anchor, gridcraft_model::StyleId::DEFAULT);
    for at in [CellRef::new(0, 1), CellRef::new(1, 0), CellRef::new(1, 1)] {
        assert_eq!(sheet.style_id(at), anchor, "{at:?}");
    }
}

/// One repeated covered run lying under several merges takes each merge's style in turn.
#[test]
fn ods_covered_run_under_several_merges_splits_at_their_edges() {
    let styles = format!(
        r##"{FILLED}<style:style style:name="ce2" style:family="table-cell"><style:table-cell-properties fo:background-color="#00ff00"/></style:style><style:style style:name="ce3" style:family="table-cell"><style:table-cell-properties fo:background-color="#0000ff"/></style:style>"##
    );
    let (wb, _) = read_ods(&styled_ods(
        &styles,
        r#"<table:table table:name="Data"><table:table-row><table:table-cell/><table:table-cell table:style-name="ce1" table:number-rows-spanned="2"/><table:table-cell table:style-name="ce2" table:number-rows-spanned="2"/></table:table-row><table:table-row><table:covered-table-cell table:number-columns-repeated="3" table:style-name="ce3"/></table:table-row></table:table>"#,
    ))
    .unwrap();
    let sheet = &wb.sheets[0];
    assert_eq!(sheet.style_id(CellRef::new(1, 1)), sheet.style_id(CellRef::new(0, 1)));
    assert_eq!(sheet.style_id(CellRef::new(1, 2)), sheet.style_id(CellRef::new(0, 2)));
    assert_ne!(sheet.style_id(CellRef::new(1, 1)), sheet.style_id(CellRef::new(1, 2)));
    // A covered cell under no merge keeps its own style.
    assert_ne!(sheet.style_id(CellRef::new(1, 0)), gridcraft_model::StyleId::DEFAULT);
}

/// A sheet-sized merge reads in time: covered cells aren't visited one by one afterwards.
#[test]
fn ods_huge_merge_does_not_hang() {
    let (wb, _) = read_ods(&styled_ods(
        FILLED,
        r#"<table:table table:name="Data"><table:table-row><table:table-cell table:style-name="ce1" table:number-columns-spanned="16384" table:number-rows-spanned="1048576"/></table:table-row></table:table>"#,
    ))
    .unwrap();
    assert_eq!(wb.sheets[0].merges.len(), 1);
}

/// A display factor that never shrinks below a thousand is no hang.
#[test]
fn ods_infinite_display_factor_does_not_hang() {
    let styles = r#"<number:number-style style:name="N1"><number:number number:display-factor="inf"/></number:number-style><number:number-style style:name="N2"><number:number number:display-factor="1e300"/></number:number-style><style:style style:name="ce1" style:family="table-cell" style:data-style-name="N1"/><style:style style:name="ce2" style:family="table-cell" style:data-style-name="N2"/>"#;
    let (wb, _) = read_ods(&styled_ods(
        styles,
        r#"<table:table table:name="Data"><table:table-row><table:table-cell table:style-name="ce1" office:value-type="float" office:value="1"/><table:table-cell table:style-name="ce2" office:value-type="float" office:value="1"/></table:table-row></table:table>"#,
    ))
    .unwrap();
    let code = wb.styles.get(wb.sheets[0].style_id(CellRef::new(0, 1))).num_fmt.as_str().to_string();
    assert!(code.matches(',').count() <= 6, "{code}");
}

/// Autofilter criteria read back as they were: an OR of two conditions, blanks alone, values
/// with blanks, and two ANDed conditions on one column.
#[test]
fn ods_autofilter_criteria_round_trip() {
    use gridcraft_model::{AutoFilter, FilterCriterion as F};
    let custom =
        |a: (&str, &str), b: Option<(&str, &str)>, and: bool| F::Custom { a: (a.0.into(), a.1.into()), b: b.map(|(o, v)| (o.into(), v.into())), and };
    let criteria = vec![
        (0, custom(("<", "5"), Some((">", "10")), false)),
        (1, F::Values { values: vec![], blanks: true }),
        (2, F::Values { values: vec!["a".into(), "b".into()], blanks: true }),
        (3, custom((">=", "1"), Some(("<=", "9")), true)),
        (4, F::Values { values: vec!["x".into()], blanks: false }),
        (5, custom(("=", "abc*"), Some(("<>", "*z")), true)),
        (6, custom((">", "100"), None, true)),
        (7, custom(("=", "East"), Some(("=", "West")), false)),
        (8, custom(("=", "a?c"), None, true)),
    ];
    let mut wb = gridcraft_model::Workbook::new();
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    for c in 0..9 {
        sh.set_value(CellRef::new(0, c), text("h"));
        sh.set_value(CellRef::new(1, c), Value::Number(1.0));
    }
    sh.autofilter = Some(AutoFilter { range: RangeRef::new(CellRef::new(0, 0), CellRef::new(1, 8)), criteria: criteria.clone() });
    let bytes = write_ods(&wb).unwrap();
    let (back, _) = read_ods(&bytes).unwrap();
    assert_eq!(back.sheets[0].autofilter.as_ref().map(|f| f.criteria.clone()), Some(criteria));
    // Numbers are compared as numbers, prefixes as prefixes.
    let mut content = String::new();
    std::io::Read::read_to_string(&mut zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap().by_name("content.xml").unwrap(), &mut content)
        .unwrap();
    assert!(content.contains(r#"table:value="100" table:data-type="number" table:operator="&gt;""#), "{content}");
    assert!(content.contains(r#"table:value="abc" table:operator="begins""#));
}

/// What the writer used to lose: a link into a sheet whose name needs quotes, a link on an
/// error cell, a hidden name, and a picture that is neither PNG, JPEG nor GIF.
#[test]
fn ods_writer_keeps_quoted_links_error_links_hidden_names_and_picture_types() {
    use gridcraft_model::{Anchor, DefinedName, Hyperlink, Image};
    let mut wb = gridcraft_model::Workbook::new();
    wb.sheets.push(std::sync::Arc::new(gridcraft_model::Sheet::new("Q1 Sales")));
    let link = |target: &str| Hyperlink { target: target.into(), tooltip: None, external: false };
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    sh.set_value(CellRef::new(0, 0), text("go"));
    sh.hyperlinks.insert(CellRef::new(0, 0), link("'Q1 Sales'!B2"));
    sh.set_value(CellRef::new(0, 1), Value::Error(gridcraft_core::CellError::Div0));
    sh.hyperlinks.insert(CellRef::new(0, 1), link("Sheet1!A1"));
    sh.images.push(Image {
        id: 1,
        anchor: Anchor::default(),
        data: std::sync::Arc::new(b"<svg/>".to_vec()),
        mime: "image/svg+xml".into(),
        alt: String::new(),
    });
    wb.names.push(DefinedName { name: "Secret".into(), scope: None, formula: "Sheet1!$A$1".into(), comment: String::new(), hidden: true });
    let bytes = write_ods(&wb).unwrap();
    let (back, _) = read_ods(&bytes).unwrap();
    let target = |c| back.sheets[0].hyperlinks.get(&CellRef::new(0, c)).map(|h| h.target.clone());
    assert_eq!(target(0).as_deref(), Some("'Q1 Sales'!B2"));
    assert_eq!(target(1).as_deref(), Some("Sheet1!A1"));
    assert!(back.names.iter().any(|n| n.name == "Secret"));
    let zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    assert!(zip.file_names().any(|n| n.starts_with("Pictures/") && n.ends_with(".svg")), "{:?}", zip.file_names().collect::<Vec<_>>());
}

/// The scroll position survives with and without frozen panes.
#[test]
fn ods_scroll_position_round_trips() {
    let mut wb = gridcraft_model::Workbook::new();
    wb.sheets.push(std::sync::Arc::new(gridcraft_model::Sheet::new("Frozen")));
    std::sync::Arc::make_mut(&mut wb.sheets[0]).view_top_left = CellRef::new(40, 3);
    let frozen = std::sync::Arc::make_mut(&mut wb.sheets[1]);
    frozen.freeze = Some((2, 1));
    frozen.view_top_left = CellRef::new(30, 5);
    let (back, _) = read_ods(&write_ods(&wb).unwrap()).unwrap();
    assert_eq!(back.sheets[0].view_top_left, CellRef::new(40, 3));
    assert_eq!(back.sheets[1].freeze, Some((2, 1)));
    assert_eq!(back.sheets[1].view_top_left, CellRef::new(30, 5));
}

/// A cached copy of another document's sheet is skipped as it opens: it is no sheet, isn't
/// counted against the import limits, and warns of no renamed sheet.
#[test]
fn ods_external_source_tables_are_skipped() {
    let big = r#"<table:table-row table:number-rows-repeated="1048576"><table:table-cell table:number-columns-repeated="5" office:value-type="float" office:value="1"/></table:table-row>"#;
    let (wb, report) = read_ods(&ods(&format!(
        r#"<table:table table:name="Data"><table:table-row><table:table-cell office:value-type="float" office:value="1"/></table:table-row></table:table><table:table table:name="'file:///tmp/other.ods'#Sheet1">{big}</table:table><table:table table:name="Second"><table:table-row><table:table-cell office:value-type="float" office:value="2"/></table:table-row></table:table>"#
    )))
    .unwrap();
    assert_eq!(wb.sheets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Data", "Second"]);
    assert_eq!(wb.sheets[1].value(CellRef::new(0, 0)), Value::Number(2.0));
    assert!(!report.warnings.iter().any(|w| w.contains("renamed")), "{:?}", report.warnings);
}

/// Optimal one-line rows stay one line high when a manual run at the end sets a taller default,
/// and columns that state no width store nothing.
#[test]
fn ods_line_runs_keep_their_own_size_against_the_defaults() {
    let styles = r#"<style:style style:name="ro1" style:family="table-row"><style:table-row-properties style:row-height="0.452cm" style:use-optimal-row-height="true"/></style:style><style:style style:name="ro2" style:family="table-row"><style:table-row-properties style:row-height="1cm" style:use-optimal-row-height="false"/></style:style>"#;
    let (wb, _) = read_ods(&styled_ods(
        styles,
        r#"<table:table table:name="Data"><table:table-column table:number-columns-repeated="16384"/><table:table-row table:style-name="ro1" table:number-rows-repeated="3"><table:table-cell office:value-type="float" office:value="1"/></table:table-row><table:table-row table:style-name="ro2" table:number-rows-repeated="1048573"><table:table-cell/></table:table-row></table:table>"#,
    ))
    .unwrap();
    let sh = &wb.sheets[0];
    let one_line = sh.rows.get(&0).and_then(|i| i.size).unwrap_or(sh.default_row_height);
    assert!(one_line < sh.default_row_height - 10.0, "{one_line} vs {}", sh.default_row_height);
    assert!(sh.rows.get(&0).is_some_and(|i| !i.custom));
    assert!(sh.cols.is_empty(), "{}", sh.cols.len());
}

/// A named style whose chain never names Default is still under it, as LibreOffice reads it:
/// it shows the Default's font size, not just its number format.
#[test]
fn ods_styles_outside_the_default_chain_inherit_the_default() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:styles><style:style style:name="Default" style:family="table-cell"><style:text-properties fo:font-size="14pt"/></style:style><style:style style:name="Lonely" style:family="table-cell"><style:text-properties fo:font-weight="bold"/></style:style></office:styles><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell table:style-name="Lonely" office:value-type="float" office:value="1"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let (wb, _) = read_ods(&make_zip(&[("mimetype", MIME), ("META-INF/manifest.xml", MANIFEST), ("content.xml", content.as_bytes())])).unwrap();
    let style = wb.styles.get(wb.sheets[0].style_id(CellRef::new(0, 0)));
    assert!(style.font.bold);
    assert_eq!(style.font.size, 14.0);
    assert_eq!(wb.styles.get(gridcraft_model::StyleId::DEFAULT).font.size, 14.0);
}

/// A merge reaching past the sheet's edge is cut at it, with a warning, rather than refusing
/// the file.
#[test]
fn ods_merges_past_the_edge_are_clipped() {
    let (wb, report) = read_ods(&row(
        r#"<table:table-cell table:number-columns-repeated="16383"/><table:table-cell table:number-columns-spanned="5" office:value-type="float" office:value="1"/>"#,
    ))
    .unwrap();
    assert_eq!(wb.sheets[0].value(CellRef::new(0, 16383)), Value::Number(1.0));
    assert!(wb.sheets[0].merges.is_empty(), "{:?}", wb.sheets[0].merges);
    assert!(report.warnings.iter().any(|w| w.contains("edge")), "{:?}", report.warnings);
    let (wb, _) = read_ods(&row(
        r#"<table:table-cell table:number-columns-repeated="16380"/><table:table-cell table:number-columns-spanned="9" table:number-rows-spanned="2" office:value-type="float" office:value="1"/>"#,
    ))
    .unwrap();
    assert_eq!(wb.sheets[0].merges, vec![RangeRef::new(CellRef::new(0, 16380), CellRef::new(1, 16383))]);
}

/// A sheet-sized merge and a validation on whole rows are written as runs of repeated cells,
/// not one element per covered cell, and read back whole; a link on an empty cell is kept.
#[test]
fn ods_writer_writes_large_merges_and_validations_as_runs() {
    use gridcraft_model::{Hyperlink, Validation};
    let mut wb = gridcraft_model::Workbook::new();
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    sh.set_value(CellRef::new(0, 0), text("big"));
    sh.merges.push(RangeRef::new(CellRef::new(0, 0), CellRef::new(99_999, 9_999)));
    let whole_rows = RangeRef::new(CellRef::new(200_000, 0), CellRef::new(200_009, 16_383));
    sh.validations.push(Validation { ranges: vec![whole_rows], ..Validation::default() });
    sh.hyperlinks.insert(CellRef::new(300_000, 3), Hyperlink { target: "https://example.com".into(), tooltip: None, external: true });
    let bytes = write_ods(&wb).unwrap();
    assert!(bytes.len() < 200_000, "{} bytes", bytes.len());
    let (back, _) = read_ods(&bytes).unwrap();
    let back = &back.sheets[0];
    assert_eq!(back.merges, vec![RangeRef::new(CellRef::new(0, 0), CellRef::new(99_999, 9_999))]);
    assert_eq!(back.validations.first().map(|v| v.ranges.clone()), Some(vec![whole_rows]));
    assert_eq!(back.hyperlinks.get(&CellRef::new(300_000, 3)).map(|h| h.target.as_str()), Some("https://example.com"));
}

/// A page field with several items chosen, and LibreOffice's Filter button row, survive.
#[test]
fn ods_pivot_page_selections_and_filter_button_round_trip() {
    use gridcraft_model::{PivotFilter, PivotTable, PivotValue};
    let mut wb = gridcraft_model::Workbook::new();
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    for (r, (region, sales)) in [("Region", 0.0), ("East", 1.0), ("West", 2.0), ("North", 3.0)].iter().enumerate() {
        sh.set_value(CellRef::new(r as u32, 0), text(region));
        if r > 0 {
            sh.set_value(CellRef::new(r as u32, 1), Value::Number(*sales));
        }
    }
    sh.set_value(CellRef::new(0, 1), text("Sales"));
    for filter_button in [true, false] {
        let mut wb = wb.clone();
        std::sync::Arc::make_mut(&mut wb.sheets[0]).pivots = vec![PivotTable {
            source: "Sheet1!$A$1:$B$4".into(),
            anchor: CellRef::new(10, 4),
            last_range: Some(RangeRef::new(CellRef::new(6, 4), CellRef::new(12, 5))),
            filters: vec![PivotFilter { source_col: "Region".into(), selected: Some(vec!["East".into(), "North".into()]) }],
            values: vec![PivotValue { source_col: "Sales".into(), name: "Sum - Sales".into(), ..PivotValue::default() }],
            filter_button,
            ..PivotTable::default()
        }];
        let (back, _) = read_ods(&write_ods(&wb).unwrap()).unwrap();
        let pt = back.sheets[0].pivots.first().cloned().unwrap();
        assert_eq!(pt.filter_button, filter_button);
        // The anchor reads back where it was, however many rows sit above it.
        assert_eq!(pt.anchor, CellRef::new(10, 4));
        assert_eq!(pt.filters.first().and_then(|f| f.selected.clone()), Some(vec!["East".to_string(), "North".to_string()]));
    }
    // The layout is stated on the row fields; with none stated, ODF's is tabular.
    use gridcraft_model::{PivotField, PivotLayout};
    for (layout, top) in [(PivotLayout::Tabular, false), (PivotLayout::Outline, true), (PivotLayout::Outline, false), (PivotLayout::Compact, true)] {
        let mut wb = wb.clone();
        std::sync::Arc::make_mut(&mut wb.sheets[0]).pivots = vec![PivotTable {
            source: "Sheet1!$A$1:$B$4".into(),
            anchor: CellRef::new(10, 4),
            rows: vec![PivotField { source_col: "Region".into(), ..PivotField::default() }],
            values: vec![PivotValue { source_col: "Sales".into(), name: "Sum - Sales".into(), ..PivotValue::default() }],
            layout,
            subtotals_top: top,
            ..PivotTable::default()
        }];
        let (back, _) = read_ods(&write_ods(&wb).unwrap()).unwrap();
        let pt = back.sheets[0].pivots.first().cloned().unwrap();
        assert_eq!((pt.layout, pt.subtotals_top), (layout, top));
    }
}

/// Limits apply as each cell is read: a row of cells each expanding to a megabyte of spaces,
/// or a note repeated over the whole sheet, is refused instead of held in memory.
#[test]
fn ods_limits_apply_while_a_row_is_read() {
    let spaces = r#"<table:table-cell office:value-type="string"><text:p><text:s text:c="1048576"/></text:p></table:table-cell>"#.repeat(400);
    assert!(matches!(read_ods(&row(&spaces)), Err(IoError::TooLarge(_))));
    let note = ods(
        r#"<table:table table:name="Data"><table:table-row table:number-rows-repeated="1048576"><table:table-cell table:number-columns-repeated="16384"><office:annotation><text:p>n</text:p></office:annotation></table:table-cell></table:table-row></table:table>"#,
    );
    assert!(matches!(read_ods(&note), Err(IoError::TooLarge(_))));
}

/// Smaller reader fixes: a link inside a note is the note's, a text result that looks like an
/// error stays text when the file says it is text, a drive path is not relative, and a filter
/// on a column outside its range is dropped.
#[test]
fn ods_reader_keeps_what_the_file_says() {
    let (wb, _) = read_ods(&row(
        r#"<table:table-cell office:value-type="string"><text:p>x</text:p><office:annotation><text:p><text:a xlink:href="https://note.example" xmlns:xlink="http://www.w3.org/1999/xlink">n</text:a></text:p></office:annotation></table:table-cell>
        <table:table-cell table:formula="of:=&quot;#Go!&quot;" office:value-type="string" calcext:value-type="string" xmlns:calcext="urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0"><text:p>#Go!</text:p></table:table-cell>
        <table:table-cell office:value-type="string"><text:p><text:a xlink:href="C:\docs\a.ods" xmlns:xlink="http://www.w3.org/1999/xlink">d</text:a></text:p></table:table-cell>"#,
    ))
    .unwrap();
    let sh = &wb.sheets[0];
    assert!(!sh.hyperlinks.contains_key(&CellRef::new(0, 0)));
    assert!(sh.comments.contains_key(&CellRef::new(0, 0)));
    assert_eq!(sh.value(CellRef::new(0, 1)), text("#Go!"));
    assert_eq!(sh.hyperlinks.get(&CellRef::new(0, 2)).map(|h| h.target.as_str()), Some("C:\\docs\\a.ods"));
    let mut wb = gridcraft_model::Workbook::new();
    std::sync::Arc::make_mut(&mut wb.sheets[0])
        .hyperlinks
        .insert(CellRef::new(0, 0), gridcraft_model::Hyperlink { target: "C:\\docs\\a.ods".into(), tooltip: None, external: true });
    let (back, _) = read_ods(&write_ods(&wb).unwrap()).unwrap();
    assert_eq!(back.sheets[0].hyperlinks.get(&CellRef::new(0, 0)).map(|h| h.target.as_str()), Some("C:\\docs\\a.ods"));
}

/// Columns to repeat on each printed page are written and nest with column groups, as rows do.
#[test]
fn ods_title_columns_round_trip() {
    let mut wb = gridcraft_model::Workbook::new();
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    sh.set_value(CellRef::new(0, 4), Value::Number(1.0));
    for c in [0, 1] {
        sh.cols.entry(c).or_default().outline = 1;
    }
    sh.print.title_cols = Some((1, 2));
    let (back, _) = read_ods(&write_ods(&wb).unwrap()).unwrap();
    assert_eq!(back.sheets[0].print.title_cols, Some((1, 2)));
    assert_eq!(back.sheets[0].cols.get(&0).map(|c| c.outline), Some(1));
}

/// Round-two reader limits: a repeated empty cell with a validation over a formatted column is
/// not stored at every position, a picture in a repeated cell spanning column styles is read
/// once, and an empty sheet's tail row style is not stored row by row.
#[test]
fn ods_repeated_extras_are_read_once() {
    let styles = r#"<style:style style:name="ce1" style:family="table-cell"><style:text-properties fo:font-weight="bold"/></style:style>"#;
    let (wb, _) = read_ods(&styled_ods(
        styles,
        r#"<table:table table:name="Data"><table:table-column table:number-columns-repeated="16384" table:default-cell-style-name="ce1"/><table:table-row table:number-rows-repeated="1048576"><table:table-cell table:number-columns-repeated="16384" table:content-validation-name="v"/></table:table-row></table:table>"#,
    ))
    .unwrap();
    assert!(wb.sheets[0].cells.is_empty());
    let pictures = r#"<table:table table:name="Data" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"><table:table-column table:default-cell-style-name="ce1"/><table:table-column/><table:table-row><table:table-cell table:number-columns-repeated="2"><draw:frame svg:width="1cm" svg:height="1cm"><draw:image><office:binary-data>iVBORw0KGgo=</office:binary-data></draw:image></draw:frame></table:table-cell></table:table-row></table:table>"#;
    let flat = format!(
        r#"<?xml version="1.0"?><office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:mimetype="application/vnd.oasis.opendocument.spreadsheet"><office:automatic-styles>{styles}</office:automatic-styles><office:body><office:spreadsheet>{pictures}</office:spreadsheet></office:body></office:document>"#
    );
    let (wb, _) = crate::read_fods(flat.as_bytes()).unwrap();
    assert_eq!(wb.sheets[0].images.len(), 1);
    let (wb, report) = read_ods(&styled_ods(
        styles,
        r#"<table:table table:name="Data"><table:table-row table:number-rows-repeated="1048576" table:default-cell-style-name="ce1"><table:table-cell/></table:table-row></table:table>"#,
    ))
    .unwrap();
    assert!(wb.sheets[0].rows.len() < 10, "{}", wb.sheets[0].rows.len());
    assert!(!report.warnings.iter().any(|w| w.contains("row and column sizes")), "{:?}", report.warnings);
}

/// What round two's writer review found missing: a table keeps its name (as a database range),
/// shape text keeps its spaces, the calculation settings say how the workbook calculates, and a
/// row's style reaches past the written cells.
#[test]
fn ods_writer_round_two() {
    use gridcraft_model::{Shape, ShapeKind, Table};
    let mut wb = gridcraft_model::Workbook::new();
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    sh.set_value(CellRef::new(0, 0), text("h"));
    sh.set_value(CellRef::new(1, 0), Value::Number(1.0));
    sh.tables.push(Table {
        id: 1,
        name: "Sales".into(),
        range: RangeRef::new(CellRef::new(0, 0), CellRef::new(1, 0)),
        header_row: true,
        totals_row: false,
        columns: vec![],
        style: String::new(),
        banded_rows: false,
        banded_cols: false,
        first_col: false,
        last_col: false,
        filter_button: true,
        filter: vec![],
        table_type: None,
        connection_id: None,
    });
    sh.shapes.push(Shape {
        id: 1,
        kind: ShapeKind::TextBox,
        anchor: gridcraft_model::Anchor { width: 100.0, height: 40.0, ..Default::default() },
        fill: gridcraft_model::Color::Auto,
        line: gridcraft_model::Color::Auto,
        text: "a  b".into(),
        format: gridcraft_model::ShapeFormat::default(),
        math: Default::default(),
    });
    let bold = wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| s.font.bold = true);
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    sh.rows.entry(4).or_default().style = Some(bold);
    let bytes = write_ods(&wb).unwrap();
    let mut content = String::new();
    std::io::Read::read_to_string(&mut zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap().by_name("content.xml").unwrap(), &mut content)
        .unwrap();
    assert!(content.contains(r#"table:case-sensitive="false""#));
    assert!(content.contains(r#"<table:table-cell table:number-columns-repeated="16383"/>"#), "row style filler");
    let (back, _) = read_ods(&bytes).unwrap();
    assert!(back.names.iter().any(|n| n.name == "Sales"));
    assert_eq!(back.sheets[0].shapes.first().map(|s| s.text.as_str()), Some("a  b"));
}

/// A page wider than tall with no stated orientation is landscape, and its paper is the page
/// turned: a 17×11 in page is landscape Tabloid, not portrait Ledger printed sideways.
#[test]
fn ods_wide_page_without_orientation_is_turned_paper() {
    let flat = r#"<?xml version="1.0"?><office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:mimetype="application/vnd.oasis.opendocument.spreadsheet"><office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="17in" fo:page-height="11in"/></style:page-layout><style:style style:name="ta1" style:family="table" style:master-page-name="Default"/></office:automatic-styles><office:master-styles><style:master-page style:name="Default" style:page-layout-name="pm1"/></office:master-styles><office:body><office:spreadsheet><table:table table:name="S" table:style-name="ta1"><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document>"#;
    let (wb, _) = crate::read_fods(flat.as_bytes()).unwrap();
    let p = &wb.sheets[0].print;
    assert_eq!(p.orientation, gridcraft_model::Orientation::Landscape);
    assert_eq!(p.paper, "Tabloid");
}

/// Round-three reader limits: each position's copy of a repeated note counts as text, and an
/// empty unstyled cell spanning many column styles isn't split at their edges.
#[test]
fn ods_repeated_notes_and_column_runs_are_bounded() {
    let styles = r#"<style:style style:name="ce1" style:family="table-cell"><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="ce2" style:family="table-cell"><style:text-properties fo:font-style="italic"/></style:style>"#;
    let note = "x".repeat(100_000);
    let notes = format!(
        r#"<table:table table:name="Data"><table:table-row table:number-rows-repeated="100"><table:table-cell table:number-columns-repeated="4000"><office:annotation><text:p>{note}</text:p></office:annotation></table:table-cell></table:table-row></table:table>"#
    );
    assert!(matches!(read_ods(&styled_ods(styles, &notes)), Err(crate::IoError::TooLarge(_))));
    let columns: String = (0..16384).map(|i| format!(r#"<table:table-column table:default-cell-style-name="ce{}"/>"#, 1 + i % 2)).collect();
    let rows = r#"<table:table-row><table:table-cell table:number-columns-repeated="16384"/></table:table-row>"#.repeat(2500);
    let (wb, _) = read_ods(&styled_ods(styles, &format!(r#"<table:table table:name="Data">{columns}{rows}</table:table>"#))).unwrap();
    assert!(wb.sheets[0].cells.is_empty());
}

/// Page layouts kept for the sheets that use them count against one limit over the document.
#[test]
fn ods_kept_page_layouts_are_bounded() {
    let filler = "<a/>".repeat(800_000);
    // Each in its own styles block, as one element may hold only so many.
    let styles: String = (0..3)
        .map(|i| format!(r#"<style:page-layout style:name="pm{i}">{filler}</style:page-layout>"#))
        .collect::<Vec<_>>()
        .join("</office:automatic-styles><office:automatic-styles>");
    let (_, report) =
        read_ods(&styled_ods(&styles, r#"<table:table table:name="Data"><table:table-row><table:table-cell/></table:table-row></table:table>"#))
            .unwrap();
    assert!(report.warnings.iter().any(|w| w.contains("page styles")), "{:?}", report.warnings);
}

/// An autofilter holds one criterion per column of its range: many conditions on columns
/// outside it are dropped at once, and a column's second custom condition joins its first.
#[test]
fn ods_filter_conditions_are_keyed_by_column() {
    let conds: String =
        (0..200_000).map(|n| format!(r#"<table:filter-condition table:field-number="{n}" table:operator="&gt;" table:value="1"/>"#)).collect();
    let content = format!(
        r#"<table:table table:name="S"><table:table-row><table:table-cell/></table:table-row></table:table><table:database-ranges><table:database-range table:target-range-address="S.A1:S.B2" table:display-filter-buttons="true"><table:filter><table:filter-and>{conds}<table:filter-condition table:field-number="0" table:operator="&lt;" table:value="9"/></table:filter-and></table:filter></table:database-range></table:database-ranges>"#
    );
    let (wb, report) = read_ods(&styled_ods("", &content)).unwrap();
    let filter = wb.sheets[0].autofilter.as_ref().unwrap();
    assert_eq!(filter.criteria.len(), 2);
    assert!(matches!(&filter.criteria[0], (0, gridcraft_model::FilterCriterion::Custom { b: Some(_), .. })), "{:?}", filter.criteria[0]);
    assert!(report.warnings.iter().any(|w| w.contains("outside their range")));
}

/// An autofilter whose criteria ODF can't hold (a fill colour) is written without a filter
/// element, which may not be empty.
#[test]
fn ods_autofilter_without_odf_criteria_writes_no_filter() {
    let mut wb = gridcraft_model::Workbook::new();
    let sheet = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    sheet.autofilter = Some(gridcraft_model::AutoFilter {
        range: RangeRef::new(CellRef::new(0, 0), CellRef::new(3, 1)),
        criteria: vec![(0, gridcraft_model::FilterCriterion::FillColor(gridcraft_model::style::Color::Rgb(0xFFFF00)))],
    });
    let bytes = write_ods(&wb).unwrap();
    let content = String::from_utf8(crate::package::Package::open(&bytes).unwrap().read("content.xml").unwrap().unwrap()).unwrap();
    assert!(content.contains("table:database-range ") && !content.contains("table:filter"), "{content}");
}

/// Round-five reader fixes: a column's third filter condition warns instead of vanishing,
/// content past column XFD after an empty cell warns, too many namespace declarations in
/// scope are refused, and pivots in separate lists get distinct ids.
#[test]
fn ods_reader_round_five() {
    let filter = r#"<table:table table:name="S"><table:table-row><table:table-cell/></table:table-row></table:table><table:database-ranges><table:database-range table:target-range-address="S.A1:S.B2" table:display-filter-buttons="true"><table:filter><table:filter-and><table:filter-condition table:field-number="0" table:operator="&gt;" table:value="1"/><table:filter-condition table:field-number="0" table:operator="&lt;" table:value="9"/><table:filter-condition table:field-number="0" table:operator="!=" table:value="5"/></table:filter-and></table:filter></table:database-range></table:database-ranges>"#;
    let (_, report) = read_ods(&styled_ods("", filter)).unwrap();
    assert!(report.warnings.iter().any(|w| w.contains("combine conditions")), "{:?}", report.warnings);
    let past = r#"<table:table table:name="S"><table:table-row><table:table-cell table:number-columns-repeated="16384"/><table:table-cell/><table:table-cell office:value-type="float" office:value="1"/></table:table-row></table:table>"#;
    let (_, report) = read_ods(&styled_ods("", past)).unwrap();
    assert!(report.warnings.iter().any(|w| w.contains("beyond column XFD")), "{:?}", report.warnings);
    let decls: String = (0..200).map(|i| format!(r#" xmlns:p{i}="urn:x:{i}""#)).collect();
    let nested = format!(
        r#"<table:table table:name="S">{}<table:table-row><table:table-cell/></table:table-row>{}</table:table>"#,
        format!("<table:table-row-group{decls}>").repeat(6),
        "</table:table-row-group>".repeat(6)
    );
    assert!(matches!(read_ods(&styled_ods("", &nested)), Err(IoError::TooLarge(_))));
    let pivot = r#"<table:data-pilot-tables><table:data-pilot-table table:name="P" table:target-range-address="S.D1:S.E3"><table:source-cell-range table:cell-range-address="S.A1:S.B3"/></table:data-pilot-table></table:data-pilot-tables>"#;
    let two = format!(
        r#"<table:table table:name="S"><table:table-row><table:table-cell office:value-type="string"><text:p>a</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>b</text:p></table:table-cell></table:table-row></table:table>{pivot}{pivot}"#
    );
    let (wb, _) = read_ods(&styled_ods("", &two)).unwrap();
    let ids: Vec<u32> = wb.sheets[0].pivots.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![1, 2]);
}

/// A matrix formula's other cells hold its cached results: they are the formula's to fill, so
/// they come back empty, not as constants blocking it.
#[test]
fn ods_matrix_results_are_not_constants() {
    let content = r#"<table:table table:name="S"><table:table-row><table:table-cell table:formula="of:=SEQUENCE(3)" table:number-matrix-columns-spanned="1" table:number-matrix-rows-spanned="3" office:value-type="float" office:value="1"><text:p>1</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="7"><text:p>7</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="float" office:value="2"><text:p>2</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="float" office:value="3"><text:p>3</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="float" office:value="4"><text:p>4</text:p></table:table-cell></table:table-row></table:table>"#;
    let (wb, _) = read_ods(&styled_ods("", content)).unwrap();
    let sh = &wb.sheets[0];
    let v = |r, c| sh.cells.get(CellRef::new(r, c)).map(|c| c.value.clone()).unwrap_or(Value::Empty);
    assert!(sh.cells.get(CellRef::new(0, 0)).and_then(|c| c.formula.as_ref()).is_some());
    assert_eq!((v(1, 0), v(2, 0)), (Value::Empty, Value::Empty));
    // Outside the matrix, constants stay.
    assert_eq!((v(0, 1), v(3, 0)), (Value::Number(7.0), Value::Number(4.0)));
}

/// Round-six reader fixes: an OR after a column's condition, and an OR holding more than
/// conditions, warn instead of being read narrower; a pivot on a sheet renamed on import is
/// still placed.
#[test]
fn ods_reader_round_six() {
    let filter = |inner: &str| {
        format!(
            r#"<table:table table:name="S"><table:table-row><table:table-cell/></table:table-row></table:table><table:database-ranges><table:database-range table:target-range-address="S.A1:S.B2" table:display-filter-buttons="true"><table:filter>{inner}</table:filter></table:database-range></table:database-ranges>"#
        )
    };
    for inner in [
        r#"<table:filter-and><table:filter-condition table:field-number="0" table:operator="&gt;" table:value="1"/><table:filter-or><table:filter-condition table:field-number="0" table:operator="&lt;" table:value="5"/><table:filter-condition table:field-number="0" table:operator="=" table:value="9"/></table:filter-or></table:filter-and>"#,
        r#"<table:filter-or><table:filter-and><table:filter-condition table:field-number="0" table:operator="=" table:value="1"/><table:filter-condition table:field-number="1" table:operator="=" table:value="2"/></table:filter-and><table:filter-condition table:field-number="0" table:operator="=" table:value="3"/></table:filter-or>"#,
    ] {
        let (_, report) = read_ods(&styled_ods("", &filter(inner))).unwrap();
        assert!(report.warnings.iter().any(|w| w.contains("combine conditions")), "{inner}: {:?}", report.warnings);
    }
    let cell = |t: &str| format!(r#"<table:table-cell office:value-type="string"><text:p>{t}</text:p></table:table-cell>"#);
    let sheets = format!(
        r#"<table:table table:name="Data"><table:table-row>{}{}</table:table-row></table:table><table:table table:name="Data"><table:table-row><table:table-cell/></table:table-row></table:table><table:data-pilot-tables><table:data-pilot-table table:name="P" table:target-range-address="Data.D1:Data.E3"><table:source-cell-range table:cell-range-address="Data.A1:Data.B3"/></table:data-pilot-table></table:data-pilot-tables>"#,
        cell("a"),
        cell("b")
    );
    let (wb, report) = read_ods(&styled_ods("", &sheets)).unwrap();
    assert_eq!(wb.sheets[0].pivots.len(), 1, "{:?}", report.warnings);
}

/// A report drawn at its anchor with its page fields below (no room above) reads back with its
/// body where it was drawn; a page field on no source column isn't written.
#[test]
fn ods_pivot_drawn_near_the_top_keeps_its_body() {
    use gridcraft_model::{PivotFilter, PivotTable, PivotValue};
    let mut wb = gridcraft_model::Workbook::new();
    let sh = std::sync::Arc::make_mut(&mut wb.sheets[0]);
    for (c, h) in ["Region", "Product", "Sales"].iter().enumerate() {
        sh.set_value(CellRef::new(0, c as u32), text(h));
    }
    sh.set_value(CellRef::new(1, 0), text("East"));
    sh.set_value(CellRef::new(1, 1), text("Pen"));
    sh.set_value(CellRef::new(1, 2), Value::Number(1.0));
    let page = |name: &str| PivotFilter { source_col: name.into(), selected: None };
    for (filters, body) in [(vec![page("Region"), page("Product")], 5), (vec![page("Gone")], 2)] {
        let mut wb = wb.clone();
        std::sync::Arc::make_mut(&mut wb.sheets[0]).pivots = vec![PivotTable {
            source: "Sheet1!$A$1:$C$2".into(),
            anchor: CellRef::new(2, 5),
            last_range: Some(RangeRef::new(CellRef::new(2, 5), CellRef::new(9, 6))),
            filters,
            values: vec![PivotValue { source_col: "Sales".into(), name: "Sum - Sales".into(), ..PivotValue::default() }],
            ..PivotTable::default()
        }];
        let (back, _) = read_ods(&write_ods(&wb).unwrap()).unwrap();
        let pt = back.sheets[0].pivots.first().cloned().unwrap();
        assert_eq!(pt.anchor, CellRef::new(body, 5));
    }
}

/// A sheet name GridCraft can't hold (longer than 31 characters) is shortened once the file is
/// read, and the formulas and names that refer to it follow it.
#[test]
fn ods_renamed_sheets_keep_their_references() {
    let long = "A sheet name much longer than thirty-one";
    let content = format!(
        r#"<table:table table:name="{long}"><table:table-row><table:table-cell office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell></table:table-row></table:table><table:table table:name="Other"><table:table-row><table:table-cell table:formula="of:=['{long}'.A1]*2" office:value-type="float" office:value="10"><text:p>10</text:p></table:table-cell></table:table-row></table:table><table:named-expressions><table:named-range table:name="Five" table:cell-range-address="'{long}'.$A$1"/></table:named-expressions>"#
    );
    let (wb, report) = read_ods(&styled_ods("", &content)).unwrap();
    let short = wb.sheets[0].name.clone();
    assert!(short.chars().count() <= 31 && short != long, "{short}");
    assert!(report.warnings.iter().any(|w| w.contains("renamed")));
    let f = wb.sheets[1].cells.get(CellRef::new(0, 0)).and_then(|c| c.formula.clone()).unwrap();
    assert!(f.text.contains(&short), "{}", f.text);
    let five = wb.names.iter().find(|n| n.name == "Five").unwrap();
    assert!(five.formula.contains(&short), "{}", five.formula);
}

/// Decoded text counts where it is decoded: a run of spaces expanding in every error cell, or
/// in cells a matrix formula's results replace, is refused at the text limit.
#[test]
fn ods_decoded_text_is_counted() {
    let wide = r#"<table:table-cell calcext:value-type="error" xmlns:calcext="urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0"><text:p><text:s text:c="1048576"/></text:p></table:table-cell>"#.repeat(300);
    assert!(matches!(read_ods(&row(&wide)), Err(IoError::TooLarge(_))));
}

/// A matrix ending above a row doesn't hide the one that holds it.
#[test]
fn ods_matrix_after_an_ended_one_still_holds_its_cells() {
    let m = |f: &str, c: u32, r: u32| {
        format!(
            r#"<table:table-cell table:formula="of:={f}" table:number-matrix-columns-spanned="{c}" table:number-matrix-rows-spanned="{r}" office:value-type="float" office:value="1"><text:p>1</text:p></table:table-cell>"#
        )
    };
    let num = r#"<table:table-cell office:value-type="float" office:value="9"><text:p>9</text:p></table:table-cell>"#;
    let content = format!(
        r#"<table:table table:name="S"><table:table-row><table:table-cell/>{}</table:table-row><table:table-row><table:table-cell/>{num}</table:table-row><table:table-row/><table:table-row>{}{num}{num}</table:table-row><table:table-row>{num}{num}{num}</table:table-row></table:table>"#,
        m("SEQUENCE(2)", 1, 2),
        m("SEQUENCE(2;3)", 3, 2)
    );
    let (wb, _) = read_ods(&styled_ods("", &content)).unwrap();
    let sh = &wb.sheets[0];
    for (r, c) in [(1, 1), (3, 1), (3, 2), (4, 0), (4, 1), (4, 2)] {
        assert_eq!(sh.value(CellRef::new(r, c)), Value::Empty, "{r},{c}");
    }
}

/// Two sheets the file names alike: the second is renamed, and references keep meaning the
/// first, which the file means by that name.
#[test]
fn ods_duplicate_sheet_names_keep_references_on_the_first() {
    let content = r#"<table:table table:name="Data"><table:table-row><table:table-cell office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell><table:table-cell table:formula="of:=[$Data.A1]*2" office:value-type="float" office:value="10"><text:p>10</text:p></table:table-cell></table:table-row></table:table><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="float" office:value="7"><text:p>7</text:p></table:table-cell></table:table-row></table:table>"#;
    let (wb, _) = read_ods(&styled_ods("", content)).unwrap();
    assert_eq!((wb.sheets[0].name.as_str(), wb.sheets[1].name.as_str()), ("Data", "Data (2)"));
    let f = wb.sheets[0].cells.get(CellRef::new(0, 1)).and_then(|c| c.formula.clone()).unwrap();
    assert_eq!(f.text, "Data!A1*2");
}

/// Shapes' text counts as it is decoded: a cell holding many shapes whose text expands is cut
/// at the limit, not held whole.
#[test]
fn ods_shape_text_is_counted() {
    let shape = r#"<draw:rect svg:width="1cm" svg:height="1cm"><text:p>a<text:s text:c="1048576"/>b</text:p></draw:rect>"#.repeat(400);
    let content = format!(
        r#"<table:table table:name="S" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"><table:table-row><table:table-cell>{shape}</table:table-cell></table:table-row></table:table>"#
    );
    let (wb, report) = read_ods(&styled_ods("", &content)).unwrap();
    let text: usize = wb.sheets[0].shapes.iter().map(|s| s.text.len()).sum();
    assert!(text <= 256 * 1024 * 1024, "{text}");
    assert!(report.warnings.iter().any(|w| w.contains("beyond the import limit")), "{:?}", report.warnings);
}

/// A header's height holds its spacing to the body: a 0.3 in header with 0.1 in spacing under
/// a 0.5 in page margin puts the body 0.8 in down, not 0.9 in, and is written back that way.
#[test]
fn ods_header_spacing_lies_inside_its_height() {
    let flat = r#"<?xml version="1.0"?><office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:mimetype="application/vnd.oasis.opendocument.spreadsheet"><office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="8.5in" fo:page-height="11in" fo:margin-top="0.5in" fo:margin-bottom="0.5in"/><style:header-style><style:header-footer-properties fo:min-height="0.3in" fo:margin-bottom="0.1in"/></style:header-style><style:footer-style><style:header-footer-properties fo:min-height="0.25in" fo:margin-top="0.1in"/></style:footer-style></style:page-layout><style:style style:name="ta1" style:family="table" style:master-page-name="Default"/></office:automatic-styles><office:master-styles><style:master-page style:name="Default" style:page-layout-name="pm1"><style:header><text:p>Top</text:p></style:header><style:footer><text:p>Bottom</text:p></style:footer></style:master-page></office:master-styles><office:body><office:spreadsheet><table:table table:name="S" table:style-name="ta1"><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document>"#;
    let (wb, _) = crate::read_fods(flat.as_bytes()).unwrap();
    let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
    let m = wb.sheets[0].print.margins;
    assert!(close(m[2], 0.8) && close(m[4], 0.5), "{m:?}");
    assert!(close(m[3], 0.75) && close(m[5], 0.5), "{m:?}");
    let saved = crate::write_fods(&wb, None).unwrap();
    let xml = String::from_utf8_lossy(&saved);
    assert!(xml.contains(r#"fo:min-height="0.3in" fo:margin-bottom="0.0984in""#), "{xml}");
    let (again, _) = crate::read_fods(&saved).unwrap();
    assert_eq!(again.sheets[0].print.margins.map(|v| (v * 1e4).round()), m.map(|v| (v * 1e4).round()));
}

/// A page layout with no size leaves the paper to the application (A4 or Letter by locale) and
/// prints upright whatever its orientation says: it is read as unstated, written back without
/// a size, and a page turned to landscape afterwards names Letter so it prints turned.
#[test]
fn ods_page_without_a_size_keeps_the_paper_unstated() {
    let flat = r#"<?xml version="1.0"?><office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:mimetype="application/vnd.oasis.opendocument.spreadsheet"><office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties style:print-orientation="landscape" fo:margin-top="1in"/></style:page-layout><style:style style:name="ta1" style:family="table" style:master-page-name="Default"/></office:automatic-styles><office:master-styles><style:master-page style:name="Default" style:page-layout-name="pm1"/></office:master-styles><office:body><office:spreadsheet><table:table table:name="S" table:style-name="ta1"><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document>"#;
    let (mut wb, _) = crate::read_fods(flat.as_bytes()).unwrap();
    let p = &wb.sheets[0].print;
    assert_eq!((p.paper.as_str(), p.orientation), ("", gridcraft_model::Orientation::Portrait));
    let xml = String::from_utf8(crate::write_fods(&wb, None).unwrap()).unwrap();
    assert!(!xml.contains("fo:page-width"), "{xml}");
    let (again, _) = crate::read_fods(xml.as_bytes()).unwrap();
    assert_eq!(again.sheets[0].print.paper, "");
    std::sync::Arc::make_mut(&mut wb.sheets[0]).print.orientation = gridcraft_model::Orientation::Landscape;
    let xml = String::from_utf8(crate::write_fods(&wb, None).unwrap()).unwrap();
    assert!(xml.contains(r#"fo:page-width="792pt" fo:page-height="612pt""#), "{xml}");
    // A sheet with no page style at all is unstated too.
    let (plain, _) = read_ods(&row("<table:table-cell/>")).unwrap();
    assert_eq!(plain.sheets[0].print.paper, "");
}

/// Margins a page style leaves out are LibreOffice's 2 cm, not Excel's defaults: the body of a
/// page with a 0.3 in footer and no stated margins ends 2 cm + 0.3 in from the bottom.
#[test]
fn ods_unstated_margins_are_two_centimetres() {
    let flat = r#"<?xml version="1.0"?><office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:mimetype="application/vnd.oasis.opendocument.spreadsheet"><office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties style:table-centering="none"/><style:footer-style><style:header-footer-properties fo:min-height="0.3in" fo:margin-top="0.1in"/></style:footer-style></style:page-layout><style:style style:name="ta1" style:family="table" style:master-page-name="Default"/></office:automatic-styles><office:master-styles><style:master-page style:name="Default" style:page-layout-name="pm1"><style:header style:display="false"/><style:footer><text:p>Page</text:p></style:footer></style:master-page></office:master-styles><office:body><office:spreadsheet><table:table table:name="S" table:style-name="ta1"><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document>"#;
    let (wb, _) = crate::read_fods(flat.as_bytes()).unwrap();
    let m = wb.sheets[0].print.margins.map(|v| (v * 1e4).round() / 1e4);
    assert_eq!(m, [0.7874, 0.7874, 0.7874, 1.0874, 0.7874, 0.7874]);
    // Without any page style the margins are LibreOffice's too.
    let (plain, _) = read_ods(&row("<table:table-cell/>")).unwrap();
    assert_eq!(plain.sheets[0].print.margins, [0.7874; 6]);
}

/// Turned text keeps its cell upright: the writer says `rotation-align="none"`, or LibreOffice
/// skews the cell's fill along the text (its default when the attribute is missing).
#[test]
fn ods_turned_text_keeps_its_cell_upright() {
    let styles = r#"<style:style style:name="ce1" style:family="table-cell"><style:table-cell-properties style:rotation-angle="5" style:rotation-align="none"/></style:style>"#;
    let (wb, _) = read_ods(&styled_ods(
        styles,
        r#"<table:table table:name="Data"><table:table-row><table:table-cell table:style-name="ce1"/></table:table-row></table:table>"#,
    ))
    .unwrap();
    let xml = String::from_utf8(crate::write_fods(&wb, None).unwrap()).unwrap();
    assert!(xml.contains(r#"style:rotation-angle="5" style:rotation-align="none""#), "{xml}");
}

/// The default style's languages and Asian/complex fonts survive a save: without them
/// LibreOffice takes its own locale's (en-US, zh-CN, hi-IN) and picks other CJK fonts.
#[test]
fn ods_default_languages_and_script_fonts_round_trip() {
    let styles = r#"<?xml version="1.0"?><office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"><office:font-face-decls><style:font-face style:name="CJK Face" svg:font-family="'Sample Mincho'"/></office:font-face-decls><office:styles><style:default-style style:family="table-cell"><style:text-properties fo:language="de" fo:country="DE" style:language-asian="ja" style:country-asian="JP" style:font-name-asian="CJK Face" style:language-complex="ar" style:country-complex="SA" style:font-family-complex="'Sample Naskh'"/></style:default-style></office:styles></office:document-styles>"#;
    let manifest = br#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/></manifest:manifest>"#;
    let body = content(r#"<table:table table:name="Data"><table:table-row><table:table-cell/></table:table-row></table:table>"#);
    let pkg =
        make_zip(&[("mimetype", MIME), ("META-INF/manifest.xml", manifest), ("content.xml", body.as_bytes()), ("styles.xml", styles.as_bytes())]);
    let (wb, _) = read_ods(&pkg).unwrap();
    let d = &wb.text_defaults;
    let s = |v: &Option<String>| v.clone().unwrap_or_default();
    assert_eq!((s(&d.western.language), s(&d.western.country)), ("de".into(), "DE".into()));
    assert_eq!((s(&d.asian.language), s(&d.asian.country), s(&d.asian.font)), ("ja".into(), "JP".into(), "Sample Mincho".into()));
    assert_eq!((s(&d.complex.language), s(&d.complex.country), s(&d.complex.font)), ("ar".into(), "SA".into(), "Sample Naskh".into()));
    let (again, _) = read_ods(&write_ods(&wb).unwrap()).unwrap();
    assert_eq!(&again.text_defaults, d);
}

/// A sheet marked not to print (`table:print="false"`) keeps the mark through a save.
#[test]
fn ods_sheets_left_out_of_printing_stay_out() {
    let (wb, _) = read_ods(&ods(
        r#"<table:table table:name="Notes" table:print="false"><table:table-row><table:table-cell/></table:table-row></table:table><table:table table:name="Report"><table:table-row><table:table-cell/></table:table-row></table:table>"#,
    ))
    .unwrap();
    assert!(wb.sheets[0].print.excluded && !wb.sheets[1].print.excluded);
    let (again, _) = read_ods(&write_ods(&wb).unwrap()).unwrap();
    assert!(again.sheets[0].print.excluded && !again.sheets[1].print.excluded);
}

/// A declared font's generic family is kept: LibreOffice picks a stand-in of the same kind
/// (a script face for a missing script font) only when the declaration says so.
#[test]
fn ods_font_declarations_keep_their_generic_family() {
    let doc = r#"<?xml version="1.0"?><office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:mimetype="application/vnd.oasis.opendocument.spreadsheet"><office:font-face-decls><style:font-face style:name="Hand" svg:font-family="'Sample Hand'" style:font-family-generic="script"/><style:font-face style:name="Body" svg:font-family="'Sample Serif'" style:font-family-generic="roman"/></office:font-face-decls><office:styles><style:default-style style:family="table-cell"><style:text-properties style:font-name="Body"/></style:default-style></office:styles><office:automatic-styles><style:style style:name="ce1" style:family="table-cell"><style:text-properties style:font-name="Hand"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="S"><table:table-row><table:table-cell table:style-name="ce1" office:value-type="string"><text:p>x</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>y</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document>"#;
    let (wb, _) = crate::read_fods(doc.as_bytes()).unwrap();
    let font = |wb: &gridcraft_model::Workbook, col: u32| wb.styles.get(wb.sheets[0].style_id(CellRef::new(0, col))).font.clone();
    assert_eq!((font(&wb, 0).name.as_str(), font(&wb, 0).family), ("Sample Hand", Some(4)));
    assert_eq!((font(&wb, 1).name.as_str(), font(&wb, 1).family), ("Sample Serif", Some(1)));
    let xml = String::from_utf8(crate::write_fods(&wb, None).unwrap()).unwrap();
    assert!(xml.contains(r#"svg:font-family="'Sample Hand'" style:font-family-generic="script""#), "{xml}");
    assert!(xml.contains(r#"svg:font-family="'Sample Serif'" style:font-family-generic="roman""#), "{xml}");
}

/// An optimal row height is shown as stored: LibreOffice keeps it when it opens a file, so
/// one line of 11 pt text stored as 0.1917 in is written back as that, not as a guess from
/// the font size (0.1955 in), which grew every row and moved page breaks.
#[test]
fn ods_optimal_row_heights_are_kept_as_stored() {
    let styles = r#"<style:style style:name="ro1" style:family="table-row"><style:table-row-properties style:row-height="0.1917in" style:use-optimal-row-height="true"/></style:style><style:style style:name="ro2" style:family="table-row"><style:table-row-properties style:row-height="0.3in" style:use-optimal-row-height="true"/></style:style>"#;
    let rows = r#"<table:table table:name="Data"><table:table-row table:style-name="ro1"><table:table-cell office:value-type="string"><text:p>a</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro2"><table:table-cell office:value-type="string"><text:p>b</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro1" table:number-rows-repeated="1048574"><table:table-cell/></table:table-row></table:table>"#;
    let (wb, _) = read_ods(&styled_ods(styles, rows)).unwrap();
    let xml = String::from_utf8(crate::write_fods(&wb, None).unwrap()).unwrap();
    assert!(xml.contains(r#"style:row-height="0.1917in""#), "{xml}");
    assert!(xml.contains(r#"style:row-height="0.3000in""#), "{xml}");
    assert!(!xml.contains(r#"style:row-height="0.1955in""#), "{xml}");
}

/// Fit to pages wide and tall is read in either spelling: `style:` and the `loext:` one older
/// LibreOffice versions wrote (without it a sheet printed on one page spread over several).
#[test]
fn ods_fit_to_pages_reads_the_extension_spelling() {
    let flat = r#"<?xml version="1.0"?><office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:mimetype="application/vnd.oasis.opendocument.spreadsheet"><office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="11in" fo:page-height="8.5in" style:print-orientation="landscape" loext:scale-to-X="1" loext:scale-to-Y="2"/></style:page-layout><style:style style:name="ta1" style:family="table" style:master-page-name="Default"/></office:automatic-styles><office:master-styles><style:master-page style:name="Default" style:page-layout-name="pm1"/></office:master-styles><office:body><office:spreadsheet><table:table table:name="S" table:style-name="ta1"><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document>"#;
    let (wb, _) = crate::read_fods(flat.as_bytes()).unwrap();
    assert_eq!(wb.sheets[0].print.fit_to, Some((1, 2)));
    let xml = String::from_utf8(crate::write_fods(&wb, None).unwrap()).unwrap();
    assert!(xml.contains(r#"style:scale-to-X="1" style:scale-to-Y="2""#), "{xml}");
}
