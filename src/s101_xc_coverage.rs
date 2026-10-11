//! Lossless, bounded XC5.2 coverage interpretation; no feature geometry mutation.
//! NULL syntax remains unresolved where the published product/schema disagree.
use anyhow::{ensure, Context, Result};
use roxmltree::Node;
use std::ops::Range;
const XC: &str = "http://www.iho.int/s100/xc/5.2";
const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
const GCO: &str = "http://standards.iso.org/iso/19115/-3/gco/1.0";
const MAX_COVERAGES: usize = 4096;
const MAX_LEXICAL: usize = 128;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum XcScale {
    Absent,
    Positive {
        lexical: String,
        denominator: u32,
    },
    /// Preserved, NOT admitted as a schema-valid unlimited minimum.
    UnresolvedNull {
        lexical: String,
        xsi_nil: Option<String>,
        nil_reason: Option<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct XcDataCoverage {
    pub entry_range: Range<usize>,
    pub bounding_polygon_range: Range<usize>,
    pub minimum: XcScale,
    pub optimum: XcScale,
    pub maximum: XcScale,
}
fn is(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().namespace() == Some(XC) && node.tag_name().name() == name
}
fn scale(row: Node<'_, '_>, name: &str) -> Result<XcScale> {
    let mut values = row.children().filter(|n| is(*n, name));
    let Some(value) = values.next() else {
        return Ok(XcScale::Absent);
    };
    ensure!(values.next().is_none(), "Duplicate XC coverage {name}");
    ensure!(
        value.children().all(|n| n.is_text() || n.is_comment()),
        "Nested XC scale value"
    );
    let mut lexical = String::new();
    for text in value
        .children()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
    {
        ensure!(
            text.len() <= MAX_LEXICAL.saturating_sub(lexical.len()),
            "XC scale lexical receiver budget exceeded"
        );
        lexical.push_str(text);
    }
    let xsi_nil = value.attribute((XSI, "nil")).map(str::to_owned);
    let nil_reason = value.attribute((GCO, "nilReason")).map(str::to_owned);
    ensure!(
        xsi_nil.as_ref().is_none_or(|s| s.len() <= MAX_LEXICAL)
            && nil_reason.as_ref().is_none_or(|s| s.len() <= MAX_LEXICAL),
        "XC nil attribute receiver budget exceeded"
    );
    ensure!(
        value.attributes().all(|a| matches!(
            (a.namespace(), a.name()),
            (Some(XSI), "nil") | (Some(GCO), "nilReason")
        )),
        "Unsupported XC scale attribute"
    );
    // Empty/whitespace, or explicit nil attributes are retained as an unresolved
    // syntax state; never equate an absent optional element with unbounded scale.
    if lexical.trim_matches([' ', '\t', '\n', '\r']).is_empty()
        || xsi_nil.is_some()
        || nil_reason.is_some()
    {
        return Ok(XcScale::UnresolvedNull {
            lexical,
            xsi_nil,
            nil_reason,
        });
    }
    // xs:positiveInteger collapses XML whitespace and permits '+' / leading 0.
    // Do not apply ISO8211 Part10a canonical lexical restrictions here.
    let normalized = lexical.trim_matches([' ', '\t', '\n', '\r']);
    let digits = normalized.strip_prefix('+').unwrap_or(normalized);
    ensure!(
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        "Invalid XC positiveInteger lexical"
    );
    let denominator: u32 = digits
        .parse()
        .context("XC scale exceeds u32 receiver range")?;
    ensure!(denominator > 0, "XC scale must be positiveInteger");
    Ok(XcScale::Positive {
        lexical,
        denominator,
    })
}
pub(crate) fn capture(entry: Node<'_, '_>) -> Result<Vec<XcDataCoverage>> {
    let mut rows = Vec::new();
    for row in entry.children().filter(|n| is(*n, "dataCoverage")) {
        ensure!(
            rows.len() < MAX_COVERAGES,
            "XC coverage receiver row budget exceeded"
        );
        let mut polygons = row.children().filter(|n| is(*n, "boundingPolygon"));
        let polygon = polygons.next().context("Missing XC boundingPolygon")?;
        ensure!(polygons.next().is_none(), "Duplicate XC boundingPolygon");
        // Preserve the polygon subtree by exact byte range, not a bbox or guessed
        // EPSG axis conversion. Feature geometry consistency is a separate step.
        rows.push(XcDataCoverage {
            entry_range: row.range(),
            bounding_polygon_range: polygon.range(),
            minimum: scale(row, "minimumDisplayScale")?,
            optimum: scale(row, "optimumDisplayScale")?,
            maximum: scale(row, "maximumDisplayScale")?,
        });
    }
    Ok(rows)
}
#[cfg(test)]
mod tests {
    use super::*;
    use roxmltree::Document;
    fn read(scales: &str) -> Result<Vec<XcDataCoverage>> {
        let xml = format!("<xc:S100_DatasetDiscoveryMetadata xmlns:xc='{XC}' xmlns:xsi='{XSI}' xmlns:gco='{GCO}'><xc:dataCoverage><xc:boundingPolygon/>{scales}</xc:dataCoverage></xc:S100_DatasetDiscoveryMetadata>");
        let doc = Document::parse(&xml)?;
        capture(doc.root_element())
    }
    #[test]
    fn xml_integer_preserves_raw_and_accepts_schema_lexical_variants() -> Result<()> {
        for raw in ["45000", "+045000", " \n45000\t"] {
            assert_eq!(
                read(&format!(
                    "<xc:minimumDisplayScale>{raw}</xc:minimumDisplayScale>"
                ))?[0]
                    .minimum,
                XcScale::Positive {
                    lexical: raw.into(),
                    denominator: 45000
                }
            );
        }
        Ok::<(), anyhow::Error>(())
    }
    #[test]
    fn missing_empty_and_nil_are_not_the_same_state() {
        assert_eq!(read("").unwrap()[0].minimum, XcScale::Absent);
        for body in [
            "<xc:minimumDisplayScale/>",
            "<xc:minimumDisplayScale xsi:nil='true'/>",
            "<xc:minimumDisplayScale gco:nilReason='unknown'/>",
        ] {
            assert!(matches!(
                read(body).unwrap()[0].minimum,
                XcScale::UnresolvedNull { .. }
            ));
        }
    }
    #[test]
    fn only_coverage_children_count_and_sibling_rows_stay_separate() {
        let xml = format!("<xc:S100_DatasetDiscoveryMetadata xmlns:xc='{XC}'><xc:minimumDisplayScale>90000</xc:minimumDisplayScale><xc:dataCoverage><xc:boundingPolygon/><xc:minimumDisplayScale>45000</xc:minimumDisplayScale></xc:dataCoverage><xc:dataCoverage><xc:boundingPolygon/><xc:minimumDisplayScale>90000</xc:minimumDisplayScale></xc:dataCoverage></xc:S100_DatasetDiscoveryMetadata>");
        let doc = Document::parse(&xml).unwrap();
        let rows = capture(doc.root_element()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].minimum, rows[1].minimum);
        assert!(rows[0].entry_range.end <= rows[1].entry_range.start);
    }
    #[test]
    fn zero_negative_duplicates_and_nested_values_reject() {
        for raw in ["0", "-1", "4 5000", "NULL", "4294967296"] {
            assert!(read(&format!(
                "<xc:minimumDisplayScale>{raw}</xc:minimumDisplayScale>"
            ))
            .is_err());
        }
        assert!(read("<xc:minimumDisplayScale>1</xc:minimumDisplayScale><xc:minimumDisplayScale>2</xc:minimumDisplayScale>").is_err());
        assert!(read("<xc:minimumDisplayScale><xc:v>1</xc:v></xc:minimumDisplayScale>").is_err());
    }
    #[test]
    fn cancellation_without_coverage_and_legacy_optional_scales_remain_captureable() {
        let xml = format!("<xc:S100_DatasetDiscoveryMetadata xmlns:xc='{XC}'/>");
        let doc = Document::parse(&xml).unwrap();
        assert!(capture(doc.root_element()).unwrap().is_empty());
        assert_eq!(read("").unwrap()[0].optimum, XcScale::Absent);
    }
}
