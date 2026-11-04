//! Function names: OpenFormula (ODF 1.2 part 2 §6, plus the `COM.MICROSOFT.` and `LEGACY.`
//! namespaces LibreOffice writes for Excel's functions) ↔ Excel names. Mapping observed by
//! converting a workbook calling every GridCraft function with LibreOffice.

/// Excel name → ODF name, where they differ and the ODF name has no prefix rule.
const RENAMES: &[(&str, &str)] = &[
    ("SKEW.P", "SKEWP"),
    ("F.DIST", "FDIST"),
    ("F.INV", "FINV"),
    ("CEILING", "COM.MICROSOFT.CEILING"),
    ("FLOOR", "COM.MICROSOFT.FLOOR"),
    ("NORMSDIST", "LEGACY.NORMSDIST"),
    ("NORMSINV", "LEGACY.NORMSINV"),
    ("TDIST", "LEGACY.TDIST"),
    ("CHIDIST", "LEGACY.CHIDIST"),
    ("CHIINV", "LEGACY.CHIINV"),
    ("CHITEST", "LEGACY.CHITEST"),
    ("FDIST", "LEGACY.FDIST"),
    ("FINV", "LEGACY.FINV"),
    ("FORMULATEXT", "FORMULA"),
];

/// Excel 2010+ functions (prefixed `_xlfn.` in XLSX) that OpenFormula has under the same name.
const ODF_NATIVE: &[&str] = &[
    "ACOT",
    "ACOTH",
    "ARABIC",
    "BASE",
    "BINOM.DIST.RANGE",
    "BITAND",
    "BITLSHIFT",
    "BITOR",
    "BITRSHIFT",
    "BITXOR",
    "COMBINA",
    "COT",
    "COTH",
    "CSC",
    "CSCH",
    "DAYS",
    "DECIMAL",
    "GAMMA",
    "GAUSS",
    "IFNA",
    "IMCOSH",
    "IMCOT",
    "IMCSC",
    "IMCSCH",
    "IMSEC",
    "IMSECH",
    "IMSINH",
    "IMTAN",
    "ISFORMULA",
    "ISOWEEKNUM",
    "MUNIT",
    "NUMBERVALUE",
    "PDURATION",
    "PERMUTATIONA",
    "PHI",
    "RRI",
    "SEC",
    "SECH",
    "SHEET",
    "SHEETS",
    "UNICHAR",
    "UNICODE",
    "XOR",
];

/// Excel functions LibreOffice writes in its `COM.MICROSOFT.` namespace, beyond the future
/// functions XLSX prefixes with `_xlfn.` (which all go there too).
const MICROSOFT: &[&str] = &[
    "AGGREGATE",
    "NETWORKDAYS.INTL",
    "WORKDAY.INTL",
    "CEILING.MATH",
    "CEILING.PRECISE",
    "ISO.CEILING",
    "FLOOR.MATH",
    "FLOOR.PRECISE",
    "MAXIFS",
    "MINIFS",
    "MODE.SNGL",
    "MODE.MULT",
    "STDEV.S",
    "STDEV.P",
    "VAR.S",
    "VAR.P",
    "RANK.EQ",
    "RANK.AVG",
    "PERCENTILE.INC",
    "PERCENTILE.EXC",
    "QUARTILE.INC",
    "QUARTILE.EXC",
    "PERCENTRANK.INC",
    "PERCENTRANK.EXC",
    "FORECAST.LINEAR",
    "COVARIANCE.P",
    "COVARIANCE.S",
    "NORM.DIST",
    "NORM.INV",
    "NORM.S.DIST",
    "NORM.S.INV",
    "T.DIST",
    "T.DIST.2T",
    "T.DIST.RT",
    "T.INV",
    "T.INV.2T",
    "T.TEST",
    "CHISQ.DIST",
    "CHISQ.DIST.RT",
    "CHISQ.INV",
    "CHISQ.INV.RT",
    "CHISQ.TEST",
    "F.DIST.RT",
    "F.INV.RT",
    "F.TEST",
    "BINOM.DIST",
    "BINOM.INV",
    "POISSON.DIST",
    "EXPON.DIST",
    "GAMMA.DIST",
    "GAMMA.INV",
    "GAMMALN.PRECISE",
    "BETA.DIST",
    "BETA.INV",
    "LOGNORM.DIST",
    "LOGNORM.INV",
    "WEIBULL.DIST",
    "HYPGEOM.DIST",
    "NEGBINOM.DIST",
    "CONFIDENCE.NORM",
    "CONFIDENCE.T",
    "Z.TEST",
    "FORECAST.ETS",
    "FORECAST.ETS.CONFINT",
    "FORECAST.ETS.SEASONALITY",
    "FORECAST.ETS.STAT",
    "CONCAT",
    "TEXTJOIN",
    "IFS",
    "SWITCH",
    "ENCODEURL",
    "WEBSERVICE",
    "FILTERXML",
    "ERF.PRECISE",
    "ERFC.PRECISE",
];

/// The ODF spelling of an Excel function name (upper-case, without `_xlfn.`).
pub(crate) fn excel_to_odf(name: &str) -> String {
    if let Some((_, odf)) = RENAMES.iter().find(|(x, _)| *x == name) {
        return (*odf).to_string();
    }
    // Newer Excel functions OpenFormula lacks (XLOOKUP, FILTER, LET…) go in the same namespace,
    // where LibreOffice versions that have them look for them.
    if MICROSOFT.contains(&name) || (crate::tables::file_function_name(name).is_some() && !ODF_NATIVE.contains(&name)) {
        return format!("COM.MICROSOFT.{name}");
    }
    name.to_string()
}

/// The Excel spelling of a function name read from an ODF formula.
pub(crate) fn odf_to_excel(name: &str) -> String {
    let u = name.to_ascii_uppercase();
    for p in ["COM.MICROSOFT.", "LEGACY.", "_XLFN._XLWS.", "_XLFN.", "_XLWS."] {
        if let Some(rest) = u.strip_prefix(p) {
            return rest.to_string();
        }
    }
    if let Some((excel, _)) = RENAMES.iter().find(|(_, odf)| *odf == u) {
        return (*excel).to_string();
    }
    match u.as_str() {
        // OpenFormula's CEILING and FLOOR take an optional mode, as Excel's .MATH variants do.
        "CEILING" => "CEILING.MATH".into(),
        "FLOOR" => "FLOOR.MATH".into(),
        "CHISQDIST" => "CHISQ.DIST".into(),
        "CHISQINV" => "CHISQ.INV".into(),
        _ => u,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for name in [
            "SUM",
            "STDEV.S",
            "SKEW.P",
            "F.DIST",
            "FDIST",
            "CEILING",
            "NORMSDIST",
            "XLOOKUP",
            "FILTER",
            "IFS",
            "VLOOKUP",
            "XOR",
            "FORMULATEXT",
            "AGGREGATE",
        ] {
            assert_eq!(odf_to_excel(&excel_to_odf(name)), name, "{name}");
        }
        assert_eq!(excel_to_odf("XLOOKUP"), "COM.MICROSOFT.XLOOKUP");
        assert_eq!(excel_to_odf("FILTER"), "COM.MICROSOFT.FILTER");
        assert_eq!(odf_to_excel("_xlfn._xlws.filter"), "FILTER");
        assert_eq!(odf_to_excel("ceiling"), "CEILING.MATH");
        assert_eq!(excel_to_odf("XOR"), "XOR");
        assert_eq!(excel_to_odf("NETWORKDAYS.INTL"), "COM.MICROSOFT.NETWORKDAYS.INTL");
    }
}
