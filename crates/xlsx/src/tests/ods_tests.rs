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
    assert_eq!(back.sheets[0].print.title_rows, Some((0, 0)));
    assert_eq!(back.sheets[0].print.print_area, Some(RangeRef::new(CellRef::new(0, 0), CellRef::new(9, 3))));
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
