//! S-124 exchange-set rules on the shared exchange-set validator.
mod common;

use common::{workspace_file, S124_DIR};
use ferrite_s100_gml::exchange::validate_exchange_set;
use ferrite_s100_gml::{Issue, Product, Severity};
use ferrite_s124::S124;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Base file name of a '/' separated path.
fn base_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

const UKHO_SET: &str = "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB";

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
    product: Product,
    rel: &'a str,
    purpose: &'a str,
    edition: u32,
    version: &'a str,
    bbox: [f64; 4],
    omit: &'a [&'a str],
}

impl<'a> Entry<'a> {
    fn new(product: Product, rel: &'a str) -> Self {
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
        let id = self.product.identifier();
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
fn ukho_s124_exchange_set() {
    let Some(folder) = workspace_file(UKHO_SET) else {
        return;
    };
    // The parent of S100_ROOT is accepted.
    let report = validate_exchange_set(&folder, None, &[&S124]).unwrap();
    assert!(report.root.ends_with("S100_ROOT"));
    assert_eq!(report.products, [Product::S124]);
    let catalogue = report.catalogue.as_ref().unwrap();
    assert_eq!(catalogue.datasets.len(), 5);
    let d = &catalogue.datasets[0];
    assert_eq!(
        d.file_name,
        "file:/S-124/DATASET_FILES/124GB00S2Y2026N43.GML"
    );
    assert_eq!(
        (
            d.purpose.as_deref(),
            d.edition_number.as_deref(),
            d.update_number.as_deref()
        ),
        (Some("newDataset"), Some("1"), Some("0"))
    );
    assert_eq!(
        (
            d.product_identifier.as_deref(),
            d.product_version.as_deref()
        ),
        (Some("S-124"), Some("2.0.0"))
    );
    assert_eq!((d.issue_date.as_deref(), d.bounding_box), (None, None));
    // File names, files, content and structure are consistent; the catalogue
    // lacks issueDate, boundingBox, producingAgency and encodingFormat.
    assert!(
        report
            .issues
            .iter()
            .all(|i| i.severity == Severity::Warning),
        "{:#?}",
        report.issues
    );
    assert_eq!(
        checks(&report.issues).into_iter().collect::<Vec<_>>(),
        [("S-124/exchange/metadata", 20)]
    );
    for field in [
        "issueDate",
        "boundingBox",
        "producingAgency",
        "encodingFormat",
    ] {
        let n = report
            .issues
            .iter()
            .filter(|i| i.message == format!("Mandatory discovery metadata {field} is missing"))
            .count();
        assert_eq!(n, 5, "{field}");
    }
    // Check 19 with the system state: none of the five is a cancellation.
    let report = validate_exchange_set(&folder, Some(&HashSet::new()), &[&S124]).unwrap();
    assert_eq!(report.issues.len(), 20);
}

#[test]
fn s124_synthetic_exchange_sets() {
    let n = |k: u32| format!("{S124_DIR}/124GB00S2Y2026N{k}.GML");
    let root = temp_root("s124-ok");
    let ds = |name: &str| format!("S-124/DATASET_FILES/{name}");
    let (a, b) = (ds("124GB00S2Y2026N43.GML"), ds("124GB00S2Y2026N44.GML"));
    if !copy_dataset(&n(43), &root, &a, |t| t) || !copy_dataset(&n(44), &root, &b, |t| t) {
        return;
    }
    let lang = "<S100_SupportFileDiscoveryMetadata><fileName>file:/S-124/SUPPORT_FILES/124GB00LANG.XML</fileName><revisionStatus>new</revisionStatus><editionNumber>1</editionNumber><dataType>XML</dataType><resourcePurpose>languagePack</resourcePurpose></S100_SupportFileDiscoveryMetadata>";
    write(&root, "S-124/SUPPORT_FILES/124GB00LANG.XML", "<x/>");
    let entries = [
        Entry::new(Product::S124, &a).xml(),
        Entry::new(Product::S124, &b).xml(),
    ];
    write(&root, "CATALOG.XML", &catalogue(&entries, lang));
    write(&root, "CATALOG.SIGN", "sig");
    let report = validate_exchange_set(&root, Some(&HashSet::new()), &[&S124]).unwrap();
    assert!(report.issues.is_empty(), "{:#?}", report.issues);

    // No catalogue at all.
    let bare = temp_root("s124-bare");
    copy_dataset(&n(43), &bare, &a, |t| t);
    let report = validate_exchange_set(&bare, None, &[&S124]).unwrap();
    assert_eq!(
        checks(&report.issues).into_iter().collect::<Vec<_>>(),
        [
            ("S-124/exchange/catalogue", 1),
            ("S-124/exchange/signature", 1)
        ]
    );
    assert!(report.issues.iter().all(|i| i.severity == Severity::Error));

    // One defect of each kind.
    let root = temp_root("s124-bad");
    copy_dataset(&n(43), &root, &a, |t| {
        t.replacen(
            "<S124:header",
            "<S124:fileReference>124GB00ABSENT.TXT</S124:fileReference><S124:fileReference>124GB00NOTE.TXT</S124:fileReference><S124:header",
            1,
        )
    });
    copy_dataset(&n(44), &root, &b, |t| t);
    let unlisted = ds("124GB00UNLISTED.GML");
    copy_dataset(&n(45), &root, &unlisted, |t| t);
    let bad_name = ds("124gb-bad.GML");
    copy_dataset(&n(45), &root, &bad_name, |t| t);
    let big = ds("124GB00BIG.GML");
    copy_dataset(&n(46), &root, &big, |t| {
        format!("{t}<!--{}-->", "x".repeat(60 * 1024))
    });
    write(&root, "S-124/SUPPORT_FILES/124GB00NOTE.TXT", "note");
    write(&root, "S-124/CATALOGUES/124_FC_2.0.0.xml", "<fc/>");
    write(&root, "S-124/CATALOG.XML", "<copy/>");
    let note = "<S100_SupportFileDiscoveryMetadata><fileName>file:/S-124/SUPPORT_FILES/124GB00NOTE.TXT</fileName><revisionStatus>new</revisionStatus><editionNumber>1</editionNumber><dataType>ASCII</dataType><resourcePurpose>supportFile</resourcePurpose></S100_SupportFileDiscoveryMetadata>";
    let entries = [
        Entry {
            purpose: "newEdition",
            edition: 2,
            version: "1.0.0",
            bbox: [0., 0., 1., 1.],
            ..Entry::new(Product::S124, &a)
        }
        .xml(),
        Entry::new(Product::S124, &bad_name).xml(),
        Entry {
            omit: &["issueDate"],
            ..Entry::new(Product::S124, &big)
        }
        .xml(),
        Entry::new(Product::S124, &ds("124GB00GONE1.GML")).xml(),
        cancellation(&b),
        cancellation(&ds("124GB00OLD1.GML")),
        cancellation(&ds("124GB00OLD2.GML")),
        cancellation(&ds("124GB00OLD2.GML")),
    ];
    write(&root, "CATALOG.XML", &catalogue(&entries, note));
    let system: HashSet<String> = [
        "124gb00old2.gml".to_string(),
        "124GB00S2Y2026N44.GML".to_string(),
    ]
    .into();
    let report = validate_exchange_set(&root, Some(&system), &[&S124]).unwrap();
    let c = checks(&report.issues);
    assert_eq!(
        c.into_iter().collect::<Vec<_>>(),
        [
            ("S-124/exchange/cancellation-file", 1), // N44 is cancelled but present
            ("S-124/exchange/cancellation-unknown", 2), // OLD1 unknown, OLD2 twice
            ("S-124/exchange/catalogue", 1),         // S-124/CATALOG.XML
            ("S-124/exchange/catalogue-metadata", 1), // the unlisted FC
            ("S-124/exchange/dataset-metadata", 1),  // UNLISTED
            ("S-124/exchange/file-name", 1),         // 124gb-bad
            ("S-124/exchange/metadata", 3),          // version, bbox; BIG issueDate
            ("S-124/exchange/purpose", 1),           // newEdition
            ("S-124/exchange/reference", 2),         // GONE1, ABSENT.TXT
            ("S-124/exchange/signature", 1),
            ("S-124/exchange/size", 1),
            ("S-124/exchange/support-file", 1), // not a language pack
        ],
        "{:#?}",
        report.issues
    );
    let warnings: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.severity == Severity::Warning)
        .map(|i| (i.check.as_str(), i.object.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(
        warnings,
        [
            ("S-124/exchange/metadata", "124GB00BIG.GML"),
            ("S-124/exchange/cancellation-unknown", "124GB00OLD1.GML"),
            ("S-124/exchange/cancellation-unknown", "124GB00OLD2.GML"),
        ]
    );
    assert!(report
        .issues
        .iter()
        .any(|i| i.message.contains("124GB00ABSENT.TXT")));
    for r in [&root, &bare] {
        let _ = std::fs::remove_dir_all(r.parent().unwrap());
    }
    let _ = std::fs::remove_dir_all(temp_root("s124-ok").parent().unwrap());
}
