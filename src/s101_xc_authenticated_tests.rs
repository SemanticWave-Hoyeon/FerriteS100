use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use ferrite_security::{authorize_datasets, AuthorizedDatasets, TrustAnchors, UnsignedPolicy};
use openssl::{hash::MessageDigest, nid::Nid, x509::X509};
const SE: &str = "http://www.iho.int/s100/se/5.2";
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    pkey::{PKey, Private},
    sign::Signer,
    x509::{
        extension::{BasicConstraints, KeyUsage},
        X509NameBuilder,
    },
};
const NOW: i64 = 1_791_043_200;
fn key(curve: Nid) -> PKey<Private> {
    PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(curve).unwrap()).unwrap()).unwrap()
}
fn certificate(
    name: &str,
    key: &PKey<Private>,
    issuer: Option<(&X509, &PKey<Private>)>,
    ca: bool,
    digital: bool,
) -> X509 {
    let mut n = X509NameBuilder::new().unwrap();
    n.append_entry_by_text("CN", name).unwrap();
    let n = n.build();
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    let serial = BigNum::from_u32(if ca { 1 } else { 2 })
        .unwrap()
        .to_asn1_integer()
        .unwrap();
    b.set_serial_number(&serial).unwrap();
    b.set_subject_name(&n).unwrap();
    b.set_issuer_name(issuer.map(|i| i.0.subject_name()).unwrap_or(&n))
        .unwrap();
    b.set_pubkey(key).unwrap();
    b.set_not_before(&Asn1Time::from_unix(NOW - 3600).unwrap())
        .unwrap();
    b.set_not_after(&Asn1Time::from_unix(NOW + 86400).unwrap())
        .unwrap();
    let mut constraints = BasicConstraints::new();
    constraints.critical();
    if ca {
        constraints.ca();
    }
    b.append_extension(constraints.build().unwrap()).unwrap();
    let mut usage = KeyUsage::new();
    usage.critical();
    if ca {
        usage.key_cert_sign().crl_sign();
    } else if digital {
        usage.digital_signature();
    } else {
        usage.key_encipherment();
    }
    b.append_extension(usage.build().unwrap()).unwrap();
    b.sign(issuer.map(|i| i.1).unwrap_or(key), MessageDigest::sha384())
        .unwrap();
    b.build()
}
fn signature(key: &PKey<Private>, bytes: &[u8]) -> Vec<u8> {
    let mut s = Signer::new(MessageDigest::sha384(), key).unwrap();
    s.update(bytes).unwrap();
    s.sign_to_vec().unwrap()
}
struct Fixture {
    dir: tempfile::TempDir,
    anchors: TrustAnchors,
    key: PKey<Private>,
    container: String,
}
impl Fixture {
    fn new(intermediate: bool, digital: bool, curve: Nid) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let rk = key(Nid::SECP384R1);
        let root = certificate("ROOT", &rk, None, true, true);
        let ik = key(Nid::SECP384R1);
        let inter = certificate("INTER", &ik, Some((&root, &rk)), true, true);
        let k = key(curve);
        let (issuer, issuer_key) = if intermediate {
            (&inter, &ik)
        } else {
            (&root, &rk)
        };
        let leaf = certificate("PRODUCER", &k, Some((issuer, issuer_key)), false, digital);
        let mut container=format!("<se:schemeAdministrator id=\"IHO\"/><se:certificate id=\"LEAF\" issuer=\"{}\">{}</se:certificate>",if intermediate{"INTER"}else{"IHO"},STANDARD.encode(leaf.to_der().unwrap()));
        if intermediate {
            container += &format!(
                "<se:certificate id=\"INTER\" issuer=\"IHO\">{}</se:certificate>",
                STANDARD.encode(inter.to_der().unwrap())
            );
        }
        let mut anchors = TrustAnchors::default();
        anchors.install_pem("IHO", &root.to_pem().unwrap()).unwrap();
        Self {
            dir,
            anchors,
            key: k,
            container,
        }
    }
    fn write(&self, name: &str, chained: bool, cert_ref: &str, status: &str) {
        let data = b"synthetic S101 capture bytes\x00\xff";
        std::fs::write(self.dir.path().join("101AA00TEST.000"), data).unwrap();
        let direct = signature(&self.key, data);
        let chain = if chained {
            format!("<xc:digitalSignatureValue><se:S100_SE_SignatureOnSignature id=\"CHAIN\" signatureRef=\"DATA\" certificateRef=\"LEAF\">{}</se:S100_SE_SignatureOnSignature></xc:digitalSignatureValue>",STANDARD.encode(signature(&self.key,&direct)))
        } else {
            String::new()
        };
        let catalogue=format!("<xc:S100_ExchangeCatalogue xmlns:xc=\"{XC}\" xmlns:se=\"{SE}\"><xc:certificates>{}</xc:certificates><xc:datasetDiscoveryMetadata><xc:S100_DatasetDiscoveryMetadata><xc:fileName>{name}</xc:fileName><xc:purpose>newDataset</xc:purpose><xc:editionNumber>1</xc:editionNumber><xc:updateNumber>0</xc:updateNumber><xc:issueDate>2026-06-18</xc:issueDate><xc:digitalSignatureReference>ECDSA-384-SHA2</xc:digitalSignatureReference><xc:digitalSignatureValue><se:S100_SE_SignatureOnData id=\"DATA\" certificateRef=\"{cert_ref}\" dataStatus=\"{status}\">{}</se:S100_SE_SignatureOnData></xc:digitalSignatureValue>{chain}</xc:S100_DatasetDiscoveryMetadata></xc:datasetDiscoveryMetadata></xc:S100_ExchangeCatalogue>",self.container,STANDARD.encode(direct));
        self.catalogue(catalogue.as_bytes());
    }
    fn catalogue(&self, bytes: &[u8]) {
        std::fs::write(self.dir.path().join("CATALOG.XML"), bytes).unwrap();
        let sign=format!("<se:StandaloneDigitalSignature xmlns:se=\"{SE}\"><se:filename>CATALOG.XML</se:filename><se:certificates>{}</se:certificates><se:digitalSignature id=\"CAT\" certificateRef=\"LEAF\">{}</se:digitalSignature></se:StandaloneDigitalSignature>",self.container,STANDARD.encode(signature(&self.key,bytes)));
        std::fs::write(self.dir.path().join("CATALOG.SIGN"), sign).unwrap();
    }
}

const ROW: &str = "<xc:dataCoverage><xc:boundingPolygon><gex:polygon xmlns:gex='http://standards.iso.org/iso/19115/-3/gex/1.0'><gml:Polygon xmlns:gml='http://www.opengis.net/gml/3.2' gml:id='coverage'><gml:exterior><gml:LinearRing><gml:posList>50 -2 50 -1 51 -1 51 -2 50 -2</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></gex:polygon></xc:boundingPolygon><xc:optimumDisplayScale>12000</xc:optimumDisplayScale><xc:maximumDisplayScale>6000</xc:maximumDisplayScale><xc:minimumDisplayScale>45000</xc:minimumDisplayScale></xc:dataCoverage>";
fn signed() -> (Fixture, PathBuf, AuthorizedDatasets) {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/101AA00TEST.000", false, "LEAF", "unencrypted");
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    f.catalogue(
        xml.replace(
            "</xc:S100_DatasetDiscoveryMetadata>",
            &format!("{ROW}</xc:S100_DatasetDiscoveryMetadata>"),
        )
        .as_bytes(),
    );
    let path = f.dir.path().join("101AA00TEST.000").canonicalize().unwrap();
    let report = authorize_datasets(
        std::slice::from_ref(&path),
        &f.anchors,
        NOW,
        UnsignedPolicy::Reject,
    )
    .unwrap();
    (f, path, report)
}
#[test]
fn authenticated_xc_capture_uses_verified_original_and_survives_live_catalogue_changes() {
    let (f, path, report) = signed();
    let snapshot = report.snapshots[&path].as_ref().unwrap().clone();
    let first = capture(&path, snapshot.path(), &report).unwrap().unwrap();
    assert!(first.is_authenticated());
    assert_eq!(first.data_coverage.len(), 1);
    assert!(matches!(
        first.data_coverage[0].minimum,
        crate::s101_xc_coverage::XcScale::Positive {
            denominator: 45000,
            ..
        }
    ));
    let retained = first.coverage_catalogue.as_ref().unwrap().bytes().to_vec();
    let polygon = first.data_coverage[0].bounding_polygon_range.clone();
    assert!(std::str::from_utf8(&retained[polygon])
        .unwrap()
        .contains("50 -2 50 -1"));
    std::fs::write(f.dir.path().join("CATALOG.XML"), b"<tampered/>").unwrap();
    assert!(authorize_datasets(
        std::slice::from_ref(&path),
        &f.anchors,
        NOW,
        UnsignedPolicy::Reject
    )
    .is_err());
    let again = capture(&path, snapshot.path(), &report).unwrap().unwrap();
    assert_eq!(again.data_coverage, first.data_coverage);
    assert_eq!(again.coverage_catalogue.as_ref().unwrap().bytes(), retained);
    drop(report);
    std::fs::remove_file(f.dir.path().join("CATALOG.XML")).unwrap();
    first.verify_resource(&path, snapshot.path()).unwrap();
}
#[test]
fn authenticated_capture_rejects_wrong_private_resource_and_snapshot_map_binding() {
    let (f, path, mut report) = signed();
    let snapshot = report.snapshots[&path].as_ref().unwrap().clone();
    let wrong = f.dir.path().join("wrong-retained.000");
    std::fs::write(&wrong, b"not the verified resource").unwrap();
    assert!(capture(&path, &wrong, &report).is_err());
    report.snapshots.insert(path.clone(), None);
    assert!(capture(&path, snapshot.path(), &report).is_err());
    report
        .snapshots
        .insert(path.clone(), Some(snapshot.clone()));
    let foreign = f.dir.path().join("foreign.000");
    std::fs::write(&foreign, b"synthetic S101 capture bytes\x00\xff").unwrap();
    let foreign = foreign.canonicalize().unwrap();
    report
        .dataset_discovery
        .insert(foreign.clone(), report.dataset_discovery[&path].clone());
    report.snapshots.insert(foreign.clone(), Some(snapshot));
    assert!(capture(&foreign, &wrong, &report).is_err());
}
#[test]
fn actual_signed_capture_detects_evidence_provenance_mix_without_minting_authority() {
    let (_f, path, report) = signed();
    let snapshot = report.snapshots[&path].as_ref().unwrap();
    let mut evidence = capture(&path, snapshot.path(), &report).unwrap().unwrap();
    let bytes = Arc::from(evidence.coverage_catalogue.as_ref().unwrap().bytes());
    evidence.coverage_catalogue = Some(CapturedCoverageCatalogue::Unverified {
        canonical_path: PathBuf::from("/unverified/CATALOG.XML"),
        bytes,
    });
    assert!(evidence.verify_resource(&path, snapshot.path()).is_err());
}
#[test]
fn retained_entry_selector_rejects_wrong_boundary_and_logical_uri() {
    let (_f, path, report) = signed();
    let DatasetDiscoveryAuthorization::Authenticated(bound) = &report.dataset_discovery[&path]
    else {
        panic!("expected verified discovery")
    };
    let proof = bound.original_authentication();
    let range = proof.discovery_range();
    assert!(capture_authenticated_rows(
        proof.catalogue_bytes(),
        range.clone(),
        proof.resource_uri()
    )
    .is_ok());
    assert!(capture_authenticated_rows(
        proof.catalogue_bytes(),
        range.start + 1..range.end,
        proof.resource_uri()
    )
    .is_err());
    assert!(
        capture_authenticated_rows(proof.catalogue_bytes(), range, "file:/foreign.000").is_err()
    );
}

/// Root may run once against a hash-guarded public original and independently
/// installed anchor; reads only, no producer files mutated or copied/re-signed.
#[test]
#[ignore = "Root-only guarded original dataset, anchor and explicit trusted verification time required"]
fn guarded_public_original_authenticated_xc_positive() -> Result<()> {
    let path = PathBuf::from(
        std::env::var_os("FERRITE_XC_SIGNED_DATASET").context("Missing guarded dataset")?,
    )
    .canonicalize()?;
    let trust = PathBuf::from(
        std::env::var_os("FERRITE_XC_TRUST_PEM")
            .context("Missing independently installed trust PEM")?,
    );
    let administrator =
        std::env::var("FERRITE_XC_TRUST_ADMIN").context("Missing explicit trust administrator")?;
    let now: i64 = std::env::var("FERRITE_XC_VERIFY_UNIX")
        .context("Missing trusted verification time")?
        .parse()?;
    let expected = std::env::var("FERRITE_XC_CATALOGUE_SHA384")
        .context("Missing original catalogue SHA384 guard")?;
    let trust_file = File::open(trust)?;
    ensure!(
        trust_file.metadata()?.len() <= 64 * 1024,
        "Test anchor budget exceeded"
    );
    let mut pem = Vec::new();
    trust_file.take(64 * 1024 + 1).read_to_end(&mut pem)?;
    ensure!(pem.len() <= 64 * 1024, "Test anchor grew beyond budget");
    let mut anchors = TrustAnchors::default();
    anchors.install_pem(&administrator, &pem)?;
    let report = authorize_datasets(
        std::slice::from_ref(&path),
        &anchors,
        now,
        UnsignedPolicy::Reject,
    )?;
    let DatasetDiscoveryAuthorization::Authenticated(bound) = &report.dataset_discovery[&path]
    else {
        bail!("Original did not authenticate")
    };
    ensure!(
        bound.catalogue_sha384() == expected,
        "Actual original catalogue guard mismatch"
    );
    let snapshot = report.snapshots[&path]
        .as_ref()
        .context("Missing verified original snapshot")?;
    let evidence =
        capture(&path, snapshot.path(), &report)?.context("Missing original XC evidence")?;
    ensure!(
        evidence.is_authenticated() && !evidence.data_coverage.is_empty(),
        "Original has no authenticated XC coverage rows"
    );
    for row in &evidence.data_coverage {
        ensure!(
            matches!(
                row.optimum,
                crate::s101_xc_coverage::XcScale::Positive { .. }
            ) && matches!(
                row.maximum,
                crate::s101_xc_coverage::XcScale::Positive { .. }
            ),
            "Original optimum/maximum not numeric positive"
        );
    }
    evidence.verify_resource(&path, snapshot.path())?;
    Ok(())
}
