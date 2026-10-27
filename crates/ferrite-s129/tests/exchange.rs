//! S-129 exchange-set rules on the shared exchange-set validator.
mod common;

use common::{workspace_file, S129_130, S129_200};
use ferrite_s100_gml::exchange::validate_exchange_set;
use ferrite_s100_gml::{read_file, Issue, ProductId, ReadLimits, Severity};
use ferrite_s129::S129;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Base file name of a '/' separated path.
fn base_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn checks(issues: &[Issue]) -> BTreeMap<&str, usize> {
    let mut m = BTreeMap::new();
    for i in issues {
        *m.entry(i.check.as_str()).or_default() += 1;
    }
    m
}

/// A fresh, empty S100_ROOT in the temporary directory.
fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrite-xc-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("S100_ROOT");
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Copies a test dataset under a new name, keeping datasetFileIdentifier in step.
fn copy_dataset(source: &str, root: &Path, rel: &str, edit: impl Fn(String) -> String) -> bool {
    let Some(path) = workspace_file(source) else {
        return false;
    };
    let text = std::fs::read_to_string(path).unwrap();
    let old = base_name(source);
    let text = text.replace(
        &format!("<S100:datasetFileIdentifier>{old}<"),
        &format!("<S100:datasetFileIdentifier>{}<", base_name(rel)),
    );
    write(root, rel, &edit(text));
    true
}

struct Entry<'a> {
    product: ProductId,
    rel: &'a str,
    purpose: &'a str,
    edition: u32,
    version: &'a str,
    bbox: [f64; 4],
    omit: &'a [&'a str],
}

impl<'a> Entry<'a> {
    fn new(product: ProductId, rel: &'a str) -> Self {
        Self {
            product,
            rel,
            purpose: "newDataset",
            edition: 1,
            version: "2.0.0",
            bbox: [-180., -90., 180., 90.],
            omit: &[],
        }
    }
    fn xml(&self) -> String {
        let [w, s, e, n] = self.bbox;
        let id = self.product.as_str();
        let fields = [
            ("compressionFlag", "<compressionFlag>false</compressionFlag>".to_string()),
            ("dataProtection", "<dataProtection>false</dataProtection>".into()),
            ("digitalSignatureReference", "<digitalSignatureReference>ECDSA-384-SHA2</digitalSignatureReference>".into()),
            ("digitalSignatureValue", "<digitalSignatureValue><S100SE:S100_SE_DigitalSignature id=\"x\" certificateRef=\"c\">MGQ=</S100SE:S100_SE_DigitalSignature></digitalSignatureValue>".into()),
            ("copyright", "<copyright>false</copyright>".into()),
            ("purpose", format!("<purpose>{}</purpose>", self.purpose)),
            ("notForNavigation", "<notForNavigation>true</notForNavigation>".into()),
            ("specificUsage", "<specificUsage>UKCM</specificUsage>".into()),
            ("editionNumber", format!("<editionNumber>{}</editionNumber>", self.edition)),
            ("updateNumber", "<updateNumber>0</updateNumber>".into()),
            ("issueDate", "<issueDate>2026-02-13</issueDate>".into()),
            ("issueTime", "<issueTime>08:00:00Z</issueTime>".into()),
            ("boundingBox", format!("<boundingBox><gex:westBoundLongitude><gco:Decimal>{w}</gco:Decimal></gex:westBoundLongitude><gex:eastBoundLongitude><gco:Decimal>{e}</gco:Decimal></gex:eastBoundLongitude><gex:southBoundLatitude><gco:Decimal>{s}</gco:Decimal></gex:southBoundLatitude><gex:northBoundLatitude><gco:Decimal>{n}</gco:Decimal></gex:northBoundLatitude></boundingBox>")),
            ("temporalExtent", "<temporalExtent><timeInstantBegin>2024-04-15T00:00:00Z</timeInstantBegin><timeInstantEnd>2024-04-20T00:00:00Z</timeInstantEnd></temporalExtent>".into()),
            ("productSpecification", format!("<productSpecification><name>{id}</name><version>{}</version><productIdentifier>{id}</productIdentifier><number>1</number></productSpecification>", self.version)),
            ("producingAgency", "<producingAgency><cit:CI_Responsibility><cit:party><cit:CI_Organisation><cit:name><gco:CharacterString>Test</gco:CharacterString></cit:name></cit:CI_Organisation></cit:party></cit:CI_Responsibility></producingAgency>".into()),
            ("encodingFormat", "<encodingFormat>GML</encodingFormat>".into()),
            ("dataCoverage", "<dataCoverage><maximumDisplayScale>1</maximumDisplayScale></dataCoverage>".into()),
        ];
        let body: String = fields
            .iter()
            .filter(|(name, _)| !self.omit.contains(name))
            .map(|(_, xml)| xml.as_str())
            .collect();
        format!(
            "<S100_DatasetDiscoveryMetadata><fileName>file:/{}</fileName>{body}</S100_DatasetDiscoveryMetadata>",
            self.rel
        )
    }
}

fn cancellation(rel: &str) -> String {
    format!("<S100_DatasetDiscoveryMetadata><fileName>file:/{rel}</fileName><purpose>cancellation</purpose><editionNumber>0</editionNumber></S100_DatasetDiscoveryMetadata>")
}

fn catalogue(datasets: &[String], support: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<S100_ExchangeCatalogue xmlns="http://www.iho.int/s100/xc/5.2" xmlns:S100SE="http://www.iho.int/s100/se/5.2" xmlns:gex="http://standards.iso.org/iso/19115/-3/gex/1.0" xmlns:gco="http://standards.iso.org/iso/19115/-3/gco/1.0" xmlns:cit="http://standards.iso.org/iso/19115/-3/cit/2.0">
<identifier><identifier>TEST</identifier><dateTime>2026-02-17T10:18:33Z</dateTime></identifier>
<datasetDiscoveryMetadata>{}</datasetDiscoveryMetadata>
<supportFileDiscoveryMetadata>{support}</supportFileDiscoveryMetadata>
<catalogueDiscoveryMetadata/>
</S100_ExchangeCatalogue>"#,
        datasets.concat()
    )
}

#[test]
fn s129_synthetic_exchange_sets() {
    let Some(path) = workspace_file(S129_130) else {
        return;
    };
    let bounds = read_file(
        &path,
        &[&ferrite_s129::rules::IDENTITY],
        None,
        ReadLimits::default(),
    )
    .unwrap()
    .geometry_bounds()
    .unwrap();
    let ds = |name: &str| format!("S-129/DATASET_FILES/{name}");
    let (a, b) = (ds("12900MCTDS130TS.gml"), ds("12900MCTDS200TS.gml"));
    let root = temp_root("s129-ok");
    if !copy_dataset(S129_130, &root, &a, |t| t) || !copy_dataset(S129_200, &root, &b, |t| t) {
        return;
    }
    let entries = [
        Entry {
            bbox: bounds,
            ..Entry::new(ferrite_s129::PRODUCT, &a)
        }
        .xml(),
        Entry::new(ferrite_s129::PRODUCT, &b).xml(),
    ];
    write(&root, "CATALOG.XML", &catalogue(&entries, ""));
    write(&root, "CATALOG.SIGN", "sig");
    let report = validate_exchange_set(&root, None, &[&S129]).unwrap();
    assert_eq!(report.products, [ferrite_s129::PRODUCT]);
    assert!(report.issues.is_empty(), "{:#?}", report.issues);

    let root = temp_root("s129-bad");
    copy_dataset(S129_130, &root, &a, |t| t);
    copy_dataset(S129_200, &root, &b, |t| t);
    let bad_name = ds("129xx-bad.gml");
    copy_dataset(S129_200, &root, &bad_name, |t| t);
    copy_dataset(S129_200, &root, &ds("12900MCTDS999.gml"), |t| t);
    write(&root, "CATALOG.SIGN", "sig");
    let entries = [
        Entry {
            version: "1.0.0",
            bbox: [bounds[0], bounds[1], bounds[0] + 0.01, bounds[1] + 0.01],
            ..Entry::new(ferrite_s129::PRODUCT, &a)
        }
        .xml(),
        Entry {
            omit: &["issueTime"],
            ..Entry::new(ferrite_s129::PRODUCT, &bad_name)
        }
        .xml(),
        Entry::new(ferrite_s129::PRODUCT, &ds("12900MCTDSGONE.gml")).xml(),
        cancellation(&b),
        cancellation(&ds("12900MCTDSOLD.gml")),
    ];
    write(&root, "CATALOG.XML", &catalogue(&entries, ""));
    let system: HashSet<String> = ["12900MCTDS200TS.GML".to_string()].into();
    let report = validate_exchange_set(&root, Some(&system), &[&S129]).unwrap();
    assert!(report
        .issues
        .iter()
        .all(|i| i.severity == Severity::Warning));
    assert_eq!(
        checks(&report.issues).into_iter().collect::<Vec<_>>(),
        [
            ("S-158-129/11", 1), // 129xx-bad.gml
            ("S-158-129/17", 1), // GONE
            ("S-158-129/18", 4), // version, bbox, issueTime, unlisted 999
            ("S-158-129/19", 1), // OLD
            ("S-158-129/20", 1), // 200TS cancelled with its file
        ],
        "{:#?}",
        report.issues
    );
    for tag in ["s129-ok", "s129-bad"] {
        let _ = std::fs::remove_dir_all(temp_root(tag).parent().unwrap());
    }
}
