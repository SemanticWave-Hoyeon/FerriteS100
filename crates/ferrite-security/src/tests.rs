use super::*;
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
        let data = b"authentic bathymetry bytes\x00\xff";
        std::fs::write(self.dir.path().join("DATA.H5"), data).unwrap();
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
    fn verify(&self, time: i64) -> Result<VerificationReport> {
        verify_exchange(self.dir.path(), &self.anchors, time)
    }
}
#[test]
fn direct_and_chained_signatures_with_intermediate_ca() {
    let f = Fixture::new(true, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let r = f.verify(NOW).unwrap();
    assert_eq!(r.resources.len(), 1);
    assert_eq!(r.resources[0].signature_ids, ["DATA", "CHAIN"]);
    assert_eq!(r.resources[0].size, 28);
}
#[test]
fn rejects_tampered_catalogue_before_parsing() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    std::fs::write(f.dir.path().join("CATALOG.XML"), b"not XML").unwrap();
    assert!(f
        .verify(NOW)
        .unwrap_err()
        .to_string()
        .contains("Invalid data signature CAT"));
}
#[test]
fn rejects_tampered_resource() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    std::fs::write(f.dir.path().join("DATA.H5"), b"tampered").unwrap();
    assert!(f
        .verify(NOW)
        .unwrap_err()
        .to_string()
        .contains("Invalid data signature DATA"));
}
#[test]
fn rejects_untrusted_expired_and_not_yet_valid_certificates() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    assert!(verify_exchange(f.dir.path(), &TrustAnchors::default(), NOW)
        .unwrap_err()
        .to_string()
        .contains("Untrusted"));
    assert!(f
        .verify(NOW + 90000)
        .unwrap_err()
        .to_string()
        .contains("expired"));
    assert!(f
        .verify(NOW - 7200)
        .unwrap_err()
        .to_string()
        .contains("not yet valid"));
}
#[test]
fn rejects_key_usage_and_wrong_curve() {
    for (digital, curve, expected) in [
        (false, Nid::SECP384R1, "does not permit"),
        (true, Nid::X9_62_PRIME256V1, "not NIST P-384"),
    ] {
        let f = Fixture::new(false, digital, curve);
        f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
        assert!(f.verify(NOW).unwrap_err().to_string().contains(expected));
    }
}
#[test]
fn rejects_unknown_references_and_unsafe_paths() {
    for (name, reference, expected) in [
        ("file:/DATA.H5", "UNKNOWN", "Unknown certificateRef"),
        ("file:/%2e%2e/DATA.H5", "LEAF", "Unsafe"),
        (
            "file://example.com/DATA.H5",
            "LEAF",
            "Invalid exchange-relative",
        ),
    ] {
        let f = Fixture::new(false, true, Nid::SECP384R1);
        f.write(name, false, reference, "unencrypted");
        assert!(f.verify(NOW).unwrap_err().to_string().contains(expected));
    }
}
#[test]
fn rejects_namespace_spoof_and_dtd() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let p = f.dir.path().join("CATALOG.XML");
    let s = std::fs::read_to_string(&p).unwrap();
    f.catalogue(s.replace(XC, "urn:spoof").as_bytes());
    assert!(f
        .verify(NOW)
        .unwrap_err()
        .to_string()
        .contains("Unsupported catalogue namespace"));
    f.catalogue(format!("<!DOCTYPE x [<!ENTITY y 'xx'>]>{s}").as_bytes());
    assert!(f.verify(NOW).is_err());
}
#[test]
fn rejects_duplicate_ids_and_signature_cycles() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let p = f.dir.path().join("CATALOG.XML");
    let s = std::fs::read_to_string(&p).unwrap();
    f.catalogue(s.replace("id=\"CHAIN\"", "id=\"DATA\"").as_bytes());
    assert!(f
        .verify(NOW)
        .unwrap_err()
        .to_string()
        .contains("Duplicate catalogue signature"));
    f.catalogue(
        s.replace("signatureRef=\"DATA\"", "signatureRef=\"CHAIN\"")
            .as_bytes(),
    );
    assert!(f
        .verify(NOW)
        .unwrap_err()
        .to_string()
        .contains("Signature reference cycle"));
}
#[cfg(unix)]
#[test]
fn rejects_symlink_escape() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::remove_file(f.dir.path().join("DATA.H5")).unwrap();
    std::os::unix::fs::symlink(outside.path(), f.dir.path().join("DATA.H5")).unwrap();
    assert!(f
        .verify(NOW)
        .unwrap_err()
        .to_string()
        .contains("symlink escapes"));
}

#[test]
fn verified_snapshot_survives_source_change_and_detects_change_before_copy() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let r = &report.resources[0];
    let snapshot = r.snapshot().unwrap();
    std::fs::write(&r.path, b"changed original").unwrap();
    assert_eq!(
        std::fs::read(snapshot.path()).unwrap(),
        b"authentic bathymetry bytes\x00\xff"
    );
    assert!(r
        .snapshot()
        .err()
        .unwrap()
        .to_string()
        .contains("changed after signature"));
}
#[test]
fn batch_authorization_requires_membership_and_explicit_unsigned_policy() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let p = f.dir.path().join("DATA.H5");
    let result = authorize_datasets(
        &[p.clone(), p.clone()],
        &f.anchors,
        NOW,
        UnsignedPolicy::Reject,
    )
    .unwrap();
    assert_eq!(result.signed_count, 1);
    assert_eq!(result.unsigned_count, 0);
    assert!(result.snapshots[&p.canonicalize().unwrap()].is_some());
    let extra = f.dir.path().join("UNLISTED.H5");
    std::fs::write(&extra, b"not signed").unwrap();
    assert!(
        authorize_datasets(&[extra], &f.anchors, NOW, UnsignedPolicy::Evaluation)
            .err()
            .unwrap()
            .to_string()
            .contains("not signed by")
    );
    std::fs::remove_file(f.dir.path().join("CATALOG.SIGN")).unwrap();
    assert!(authorize_datasets(&[p.clone()], &f.anchors, NOW, UnsignedPolicy::Reject).is_err());
    assert_eq!(
        authorize_datasets(&[p], &f.anchors, NOW, UnsignedPolicy::Evaluation)
            .unwrap()
            .unsigned_count,
        1
    );
}
#[test]
fn legacy_namespace_and_metadata_discrepancy_do_not_skip_byte_authentication() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "compressed");
    let p = f.dir.path().join("CATALOG.SIGN");
    let s = std::fs::read_to_string(&p).unwrap();
    std::fs::write(&p, s.replace(SE, "http://www.iho.int/s100/se/5.1")).unwrap();
    let report = f.verify(NOW).unwrap();
    assert_eq!(report.metadata_warnings.len(), 2);
    std::fs::write(f.dir.path().join("DATA.H5"), b"forged").unwrap();
    assert!(f.verify(NOW).is_err());
}

fn resource_fixture(
    bytes: usize,
    direct: usize,
    chained: usize,
) -> (
    tempfile::TempDir,
    PathBuf,
    Vec<Signature>,
    HashMap<String, X509>,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("PAYLOAD");
    let data = vec![0x5a; bytes];
    std::fs::write(&path, &data).unwrap();
    let k = key(Nid::SECP384R1);
    let certificate = certificate("PRODUCER", &k, None, false, true);
    let mut certs = HashMap::new();
    certs.insert("LEAF".into(), certificate);
    let signed = signature(&k, &data);
    let mut signatures = Vec::new();
    for i in 0..direct {
        signatures.push(Signature {
            id: format!("D{i}"),
            certificate: "LEAF".into(),
            bytes: signed.clone(),
            target: None,
        });
    }
    for i in 0..chained {
        let parent = signatures.last().unwrap();
        signatures.push(Signature {
            id: format!("C{i}"),
            certificate: "LEAF".into(),
            bytes: signature(&k, &parent.bytes),
            target: Some(parent.id.clone()),
        });
    }
    // Worst ordering: every parent appears after its child.
    signatures.reverse();
    (dir, path, signatures, certs)
}
fn copy_signatures(signatures: &[Signature]) -> Vec<Signature> {
    signatures
        .iter()
        .map(|s| Signature {
            id: s.id.clone(),
            certificate: s.certificate.clone(),
            bytes: s.bytes.clone(),
            target: s.target.clone(),
        })
        .collect()
}
#[test]
fn reverse_ordered_long_signature_chain_and_branch_are_all_verified() {
    let (_dir, path, signatures, certs) = resource_fixture(4096, 3, 128);
    let report = verify_resource(path.clone(), copy_signatures(&signatures), &certs).unwrap();
    assert_eq!(report.signature_ids.len(), 131);
    assert_eq!(report.size, 4096);
    let mut bad = copy_signatures(&signatures);
    bad[64].bytes[8] ^= 1;
    assert!(verify_resource(path.clone(), bad, &certs).is_err());
    let mut bad = copy_signatures(&signatures);
    bad.last_mut().unwrap().bytes.push(0);
    assert!(verify_resource(path, bad, &certs).is_err());
}
#[test]
#[ignore = "Explicit performance experiment; not part of routine regression tests"]
fn signature_verification_benchmark() {
    for (case, bytes, direct, chained) in [
        ("single", 64 * 1024 * 1024, 1, 0),
        ("multiple", 64 * 1024 * 1024, 16, 512),
    ] {
        let (_dir, path, signatures, certs) = resource_fixture(bytes, direct, chained);
        for sample in 0..3 {
            let signatures = copy_signatures(&signatures);
            let start = std::time::Instant::now();
            let r = verify_resource(path.clone(), signatures, &certs).unwrap();
            println!("BENCH case={case} bytes={bytes} direct={direct} chain={chained} sample={sample} seconds={:.6}",start.elapsed().as_secs_f64());
            assert_eq!(r.size, bytes as u64);
            assert_eq!(r.signature_ids.len(), direct + chained);
        }
    }
}

fn catalogue_fixture(f: &Fixture, scope: &str, extra: &str) -> PathBuf {
    let path = f.dir.path().join("098AA00010000.XML");
    let data = b"<local-test-interoperability-catalogue/>";
    std::fs::write(&path, data).unwrap();
    let metadata = format!("<xc:S100_CatalogueDiscoveryMetadata><xc:fileName>file:/098AA00010000.XML</xc:fileName><xc:scope>{scope}</xc:scope><xc:editionNumber>1</xc:editionNumber><xc:versionNumber>1.0.0</xc:versionNumber><xc:issueDate>2026-10-04</xc:issueDate><xc:compressionFlag>false</xc:compressionFlag><xc:productSpecification><xc:S100_ProductSpecification><xc:productIdentifier>S-98</xc:productIdentifier><xc:number>98</xc:number><xc:version>2.0.0</xc:version></xc:S100_ProductSpecification></xc:productSpecification>{extra}<xc:digitalSignatureReference>ECDSA-384-SHA2</xc:digitalSignatureReference><xc:digitalSignatureValue><se:S100_SE_SignatureOnData id=\"IC\" certificateRef=\"LEAF\" dataStatus=\"unencrypted\">{}</se:S100_SE_SignatureOnData></xc:digitalSignatureValue></xc:S100_CatalogueDiscoveryMetadata>",STANDARD.encode(signature(&f.key,data)));
    let exchange = format!("<xc:S100_ExchangeCatalogue xmlns:xc=\"{XC}\" xmlns:se=\"{SE}\"><xc:certificates>{}</xc:certificates><xc:catalogueDiscoveryMetadata>{metadata}</xc:catalogueDiscoveryMetadata></xc:S100_ExchangeCatalogue>",f.container);
    f.catalogue(exchange.as_bytes());
    path
}
fn resign_catalogue_change(f: &Fixture, old: &str, new: &str) {
    let bytes = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    assert!(bytes.contains(old));
    f.catalogue(bytes.replace(old, new).as_bytes());
}
#[test]
fn catalogue_scope_and_snapshots_are_authenticated() {
    let f = Fixture::new(true, true, Nid::SECP384R1);
    let path = catalogue_fixture(&f, "interoperabilityCatalogue", "");
    let authorized =
        authorize_catalogue(&path, CatalogueScope::Interoperability, &f.anchors, NOW).unwrap();
    assert_eq!(authorized.metadata.version_number, "1.0.0");
    assert_eq!(authorized.metadata.product_identifier, "S-98");
    assert_eq!(authorized.metadata.product_number, 98);
    assert!(!authorized.revocation_checked);
    assert_eq!(
        std::fs::read(authorized.snapshot.path()).unwrap(),
        b"<local-test-interoperability-catalogue/>"
    );
    std::fs::write(&path, b"changed after authorization").unwrap();
    assert_eq!(
        std::fs::read(authorized.snapshot.path()).unwrap(),
        b"<local-test-interoperability-catalogue/>"
    );
    assert!(authorize_catalogue(&path, CatalogueScope::Interoperability, &f.anchors, NOW).is_err());
}
#[test]
fn catalogue_activation_rejects_scope_purpose_encoding_and_metadata_errors() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    let path = catalogue_fixture(&f, "featureCatalogue", "");
    assert!(authorize_catalogue(&path, CatalogueScope::Interoperability, &f.anchors, NOW).is_err());
    for (old, new) in [
        ("<xc:issueDate>2026-10-04", "<xc:issueDate>2026-02-30"),
        ("<xc:compressionFlag>false", "<xc:compressionFlag>true"),
        ("<xc:editionNumber>1", "<xc:editionNumber>0"),
        ("<xc:versionNumber>1.0.0</xc:versionNumber>", ""),
        (
            "<xc:versionNumber>1.0.0</xc:versionNumber>",
            "<xc:versionNumber>1.0.0</xc:versionNumber><xc:versionNumber>2</xc:versionNumber>",
        ),
        ("<xc:number>98</xc:number>", ""),
        (
            "<xc:catalogueDiscoveryMetadata>",
            "<xc:datasetDiscoveryMetadata>",
        ),
    ] {
        let path = catalogue_fixture(&f, "interoperabilityCatalogue", "");
        let (old, new) = if old == "<xc:catalogueDiscoveryMetadata>" {
            ("catalogueDiscoveryMetadata", "datasetDiscoveryMetadata")
        } else {
            (old, new)
        };
        resign_catalogue_change(&f, old, new);
        assert!(
            authorize_catalogue(&path, CatalogueScope::Interoperability, &f.anchors, NOW).is_err(),
            "{old}"
        );
    }
    let path = catalogue_fixture(
        &f,
        "interoperabilityCatalogue",
        "<xc:purpose>5</xc:purpose>",
    );
    assert!(authorize_catalogue(&path, CatalogueScope::Interoperability, &f.anchors, NOW).is_err());
    catalogue_fixture(
        &f,
        "interoperabilityCatalogue",
        "<xc:purpose>newEdition</xc:purpose>",
    );
    assert!(authorize_catalogue(&path, CatalogueScope::Interoperability, &f.anchors, NOW).is_ok());
}
#[test]
fn catalogue_never_promotes_embedded_certificates_or_unsigned_resources() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    let path = catalogue_fixture(&f, "interoperabilityCatalogue", "");
    assert!(authorize_catalogue(
        &path,
        CatalogueScope::Interoperability,
        &TrustAnchors::default(),
        NOW
    )
    .is_err());
    let bytes = std::fs::read(f.dir.path().join("CATALOG.XML")).unwrap();
    std::fs::write(
        f.dir.path().join("CATALOG.XML"),
        [bytes, b" ".to_vec()].concat(),
    )
    .unwrap();
    assert!(authorize_catalogue(&path, CatalogueScope::Interoperability, &f.anchors, NOW).is_err());
    std::fs::remove_file(f.dir.path().join("CATALOG.SIGN")).unwrap();
    assert!(authorize_catalogue(&path, CatalogueScope::Interoperability, &f.anchors, NOW).is_err());
}

#[test]
fn dataset_discovery_is_bound_to_signed_catalogue_resource_and_retained_snapshot() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let path = f.dir.path().join("DATA.H5");
    let canonical = path.canonicalize().unwrap();
    let report = authorize_datasets(
        &[path.clone(), path.clone()],
        &f.anchors,
        NOW,
        UnsignedPolicy::Reject,
    )
    .unwrap();
    assert_eq!(report.signed_count, 1);
    assert_eq!(report.dataset_discovery.len(), 1);
    let DatasetDiscoveryAuthorization::Authenticated(bound) = &report.dataset_discovery[&canonical]
    else {
        panic!("expected authenticated discovery")
    };
    assert_eq!(bound.resource_path(), canonical);
    assert_eq!(
        bound.catalogue_path(),
        f.dir.path().join("CATALOG.XML").canonicalize().unwrap()
    );
    assert_eq!(
        bound.catalogue_sha384(),
        hex(&hash(
            MessageDigest::sha384(),
            &std::fs::read(bound.catalogue_path()).unwrap()
        )
        .unwrap())
    );
    assert_eq!(
        bound.resource_sha384(),
        hex(&hash(MessageDigest::sha384(), &std::fs::read(&path).unwrap()).unwrap())
    );
    assert_eq!(bound.discovery().purpose, DatasetPurpose::NewDataset);
    assert_eq!(bound.discovery().edition_number, 1);
    assert_eq!(bound.discovery().update_number, Some(0));
    assert_eq!(bound.discovery().issue_date.as_str(), "2026-06-18");
    // Mutating valid XML metadata without a new signature must fail; existing
    // typed projection and retained resource snapshot remain immutable.
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    std::fs::write(
        f.dir.path().join("CATALOG.XML"),
        xml.replace("newDataset", "cancellation"),
    )
    .unwrap();
    assert!(authorize_datasets(&[path.clone()], &f.anchors, NOW, UnsignedPolicy::Reject).is_err());
    assert_eq!(bound.discovery().purpose, DatasetPurpose::NewDataset);
    let snapshot = report.snapshots[&canonical].as_ref().unwrap();
    std::fs::write(&path, b"changed after authorization").unwrap();
    assert_eq!(
        std::fs::read(snapshot.path()).unwrap(),
        b"authentic bathymetry bytes\x00\xff"
    );
}

#[test]
fn cancellation_target_edition_and_reissue_counter_are_not_rewritten() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    for (purpose, expected) in [
        ("5", DatasetPurpose::Cancellation),
        ("reissue", DatasetPurpose::Reissue),
    ] {
        f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
        resign_catalogue_change(
            &f,
            "<xc:purpose>newDataset</xc:purpose>",
            &format!("<xc:purpose>{purpose}</xc:purpose>"),
        );
        resign_catalogue_change(
            &f,
            "<xc:editionNumber>1</xc:editionNumber>",
            "<xc:editionNumber>4</xc:editionNumber>",
        );
        resign_catalogue_change(
            &f,
            "<xc:updateNumber>0</xc:updateNumber>",
            "<xc:updateNumber>20</xc:updateNumber>",
        );
        let report = f.verify(NOW).unwrap();
        let bound = report.dataset_discovery.values().next().unwrap();
        assert_eq!(bound.discovery().purpose, expected);
        assert_eq!(bound.discovery().edition_number, 4);
        assert_eq!(bound.discovery().update_number, Some(20));
        assert_eq!(bound.discovery().issue_date.as_str(), "2026-06-18");
    }
    // Generic S-100 permits absent updateNumber; projection does not invent0.
    resign_catalogue_change(&f, "<xc:updateNumber>20</xc:updateNumber>", "");
    assert_eq!(
        f.verify(NOW)
            .unwrap()
            .dataset_discovery
            .values()
            .next()
            .unwrap()
            .discovery()
            .update_number,
        None
    );
}

#[test]
fn correctly_signed_invalid_or_ambiguous_dataset_metadata_is_rejected() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    for (old, new) in [
        (
            "<xc:purpose>newDataset</xc:purpose>",
            "<xc:purpose>unsupported</xc:purpose>",
        ),
        (
            "<xc:purpose>newDataset</xc:purpose>",
            "<xc:purpose>1</xc:purpose><xc:purpose>5</xc:purpose>",
        ),
        (
            "<xc:editionNumber>1</xc:editionNumber>",
            "<xc:editionNumber>0</xc:editionNumber>",
        ),
        (
            "<xc:editionNumber>1</xc:editionNumber>",
            "<xc:editionNumber>4294967296</xc:editionNumber>",
        ),
        (
            "<xc:updateNumber>0</xc:updateNumber>",
            "<xc:updateNumber>-1</xc:updateNumber>",
        ),
        (
            "<xc:updateNumber>0</xc:updateNumber>",
            "<xc:updateNumber>0</xc:updateNumber><xc:updateNumber>1</xc:updateNumber>",
        ),
        (
            "<xc:issueDate>2026-06-18</xc:issueDate>",
            "<xc:issueDate>2026-02-30</xc:issueDate>",
        ),
        (
            "<xc:issueDate>2026-06-18</xc:issueDate>",
            "<xc:issueDate>2026-6-18</xc:issueDate>",
        ),
        ("<xc:issueDate>2026-06-18</xc:issueDate>", ""),
    ] {
        f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
        resign_catalogue_change(&f, old, new);
        assert!(
            f.verify(NOW).is_err(),
            "accepted signed invalid metadata: {new}"
        );
    }
}

#[test]
fn unsigned_evaluation_never_projects_live_catalogue_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("DATA.H5");
    std::fs::write(&path, b"evaluation").unwrap();
    std::fs::write(dir.path().join("CATALOG.XML"), b"untrusted arbitrary XML").unwrap();
    let report = authorize_datasets(
        &[path.clone()],
        &TrustAnchors::default(),
        NOW,
        UnsignedPolicy::Evaluation,
    )
    .unwrap();
    let canonical = path.canonicalize().unwrap();
    assert!(matches!(
        report.dataset_discovery[&canonical],
        DatasetDiscoveryAuthorization::UnsignedEvaluation
    ));
    assert_eq!(report.unsigned_count, 1);
    assert!(report.snapshots[&canonical].is_none());
    assert!(authorize_datasets(
        &[path],
        &TrustAnchors::default(),
        NOW,
        UnsignedPolicy::Reject
    )
    .is_err());
}

#[test]
fn checked_discovery_rejects_rekey_missing_snapshot_and_changed_private_bytes() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let path = f.dir.path().join("DATA.H5").canonicalize().unwrap();
    let mut result =
        authorize_datasets(&[path.clone()], &f.anchors, NOW, UnsignedPolicy::Reject).unwrap();
    assert!(result.checked_dataset_discovery(&path).is_ok());
    let other = f.dir.path().join("OTHER.H5");
    result
        .dataset_discovery
        .insert(other.clone(), result.dataset_discovery[&path].clone());
    result
        .snapshots
        .insert(other.clone(), result.snapshots[&path].clone());
    assert!(result.checked_dataset_discovery(&other).is_err());
    let original = result.snapshots[&path].clone();
    result.snapshots.insert(path.clone(), None);
    assert!(result.checked_dataset_discovery(&path).is_err());
    result.snapshots.insert(path.clone(), original);
    std::fs::write(&path, b"live source changed").unwrap();
    assert!(result.checked_dataset_discovery(&path).is_ok());
    let private = result.snapshots[&path].as_ref().unwrap().path();
    std::fs::write(private, b"private bytes changed").unwrap();
    assert!(result.checked_dataset_discovery(&path).is_err());
}

#[test]
fn retained_original_proof_survives_catalogue_mutation_and_records_real_signatures() {
    let f = Fixture::new(true, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let discovery = report.dataset_discovery.values().next().unwrap();
    let proof = discovery.original_authentication();
    let original = std::fs::read(f.dir.path().join("CATALOG.XML")).unwrap();
    assert_eq!(proof.catalogue_bytes(), original);
    assert_eq!(proof.resource_uri(), "file:/DATA.H5");
    let entry = std::str::from_utf8(proof.discovery_bytes()).unwrap();
    assert!(entry.starts_with("<xc:S100_DatasetDiscoveryMetadata>"));
    assert!(entry.contains("<xc:fileName>file:/DATA.H5</xc:fileName>"));
    assert!(!format!("{report:?}").contains("catalogue_bytes"));
    assert!(!format!("{report:?}").contains("S100_ExchangeCatalogue"));
    assert_eq!(
        proof.catalogue_sha384(),
        hex(&hash(MessageDigest::sha384(), &original).unwrap())
    );
    assert_eq!(proof.verified_unix_seconds(), NOW);
    assert_eq!(proof.trust_anchor_sha256(), &report.trust_anchor_sha256);
    assert_eq!(proof.signatures().len(), 2);
    let direct = &proof.signatures()[0];
    let chained = &proof.signatures()[1];
    assert_eq!(direct.signature_target(), None);
    assert_eq!(chained.signature_target(), Some(direct.id()));
    assert_ne!(direct.der(), chained.der());
    assert_eq!(
        direct.signer_certificate_sha256(),
        chained.signer_certificate_sha256()
    );
    assert_eq!(direct.signer_certificate_sha256().len(), 64);
    let document = xml(&original).unwrap();
    let signature = document
        .descendants()
        .find(|n| is(*n, SE, "S100_SE_SignatureOnData"))
        .unwrap();
    assert_eq!(direct.der(), decode(signature).unwrap());
    let snapshot = proof.capture_resource().unwrap();
    std::fs::write(f.dir.path().join("CATALOG.XML"), b"replaced").unwrap();
    std::fs::write(f.dir.path().join("DATA.H5"), b"replaced").unwrap();
    assert_eq!(proof.catalogue_bytes(), original);
    assert_eq!(
        std::fs::read(snapshot.path()).unwrap(),
        b"authentic bathymetry bytes\x00\xff"
    );
    assert!(proof.capture_resource().is_err());
}
#[test]
fn public_report_edits_cannot_forge_snapshot_authentication() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let mut report = f.verify(NOW).unwrap();
    let forged = f.dir.path().join("OTHER.H5");
    std::fs::write(&forged, b"forged").unwrap();
    report.resources[0].path = forged.canonicalize().unwrap();
    report.resources[0].size = 6;
    report.resources[0].sha384 = hex(&hash(MessageDigest::sha384(), b"forged").unwrap());
    assert!(report.resources[0].snapshot().is_err());
    let proof = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    assert_eq!(proof.resource_size(), 28);
    assert!(proof.capture_resource().is_ok());
}

#[test]
fn signature_trace_labels_cannot_be_rewritten_on_snapshot_report() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let mut report = f.verify(NOW).unwrap();
    report.resources[0].signature_ids[0] = "OTHER".into();
    assert!(report.resources[0].snapshot().is_err());
    report.resources[0].signature_ids[0] = "DATA".into();
    report.resources[0].certificate_ids[0] = "OTHER".into();
    assert!(report.resources[0].snapshot().is_err());
}

#[test]
fn catalogue_only_authentication_is_not_missing_resource_authentication() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    std::fs::remove_file(f.dir.path().join("DATA.H5")).unwrap();
    let original = std::fs::read(f.dir.path().join("CATALOG.XML")).unwrap();
    let proof = verify_exchange_catalogue(f.dir.path(), &f.anchors, NOW).unwrap();
    assert_eq!(proof.catalogue_bytes(), original);
    assert_eq!(proof.verified_unix_seconds(), NOW);
    assert!(!proof.revocation_checked());
    assert_eq!(proof.signatures().len(), 1);
    assert_eq!(proof.discovery_view().unwrap().dataset_entries().count(), 1);
    assert!(
        f.verify(NOW).is_err(),
        "Physical resource verification must still fail"
    );
    std::fs::write(f.dir.path().join("CATALOG.XML"), b"tampered").unwrap();
    assert_eq!(proof.catalogue_bytes(), original);
    assert!(verify_exchange_catalogue(f.dir.path(), &f.anchors, NOW).is_err());
    let debug = format!("{proof:?}");
    assert!(!debug.contains("S100_ExchangeCatalogue") && !debug.contains("catalogue_bytes"));
}

#[test]
fn original_entry_preserves_namespaces_utf8_byte_range_and_signer_der() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let original = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    let modified = original
        .replace(
            "<xc:datasetDiscoveryMetadata>",
            "<!--해도 Ω--><xc:datasetDiscoveryMetadata>",
        )
        .replace("xc:", "other:")
        .replace("xmlns:xc=", "xmlns:other=");
    f.catalogue(modified.as_bytes());
    let report = f.verify(NOW).unwrap();
    let proof = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    std::fs::remove_file(f.dir.path().join("CATALOG.XML")).unwrap();
    let view = proof.original_entry_view().unwrap();
    assert_eq!(view.entry().range(), proof.discovery_range());
    assert_eq!(view.entry().tag_name().namespace(), Some(XC));
    assert_eq!(
        text(unique_child(view.entry(), XC, "fileName").unwrap()).unwrap(),
        "file:/DATA.H5"
    );
    for s in proof.signatures() {
        let cert = X509::from_der(s.signer_certificate_der()).unwrap();
        assert_eq!(cert.to_der().unwrap(), s.signer_certificate_der());
        assert_eq!(
            hex(&cert.digest(MessageDigest::sha256()).unwrap()),
            s.signer_certificate_sha256()
        );
    }
    assert!(std::sync::Arc::ptr_eq(
        &proof.signatures()[0].signer_certificate_der,
        &proof.signatures()[1].signer_certificate_der
    ));
}

#[test]
fn authenticated_discovery_layout_rejects_nested_spoof_aliases_and_wrong_namespace() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let original = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    let d = xml(original.as_bytes()).unwrap();
    let entry = d
        .descendants()
        .find(|n| is(*n, XC, "S100_DatasetDiscoveryMetadata"))
        .unwrap();
    let entry = &original[entry.range()];
    for bad in [
        original
            .replace(
                "<xc:datasetDiscoveryMetadata>",
                "<xc:fake><xc:datasetDiscoveryMetadata>",
            )
            .replace(
                "</xc:datasetDiscoveryMetadata>",
                "</xc:datasetDiscoveryMetadata></xc:fake>",
            ),
        original.replace(
            entry,
            &format!("{entry}{}", entry.replace("file:/DATA.H5", "D%41TA.H5")),
        ),
        original
            .replace(
                "<xc:S100_DatasetDiscoveryMetadata>",
                "<bad:S100_DatasetDiscoveryMetadata xmlns:bad=\"urn:spoof\">",
            )
            .replace(
                "</xc:S100_DatasetDiscoveryMetadata>",
                "</bad:S100_DatasetDiscoveryMetadata>",
            ),
    ] {
        f.catalogue(bad.as_bytes());
        assert!(verify_exchange_catalogue(f.dir.path(), &f.anchors, NOW).is_err());
        assert!(f.verify(NOW).is_err());
    }
}

#[test]
fn original_entry_cannot_select_other_range_or_resource_uri() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let proof = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let mut altered = proof.clone();
    altered.discovery_range = 0..1;
    assert!(altered.original_entry_view().is_err());
    altered = proof.clone();
    altered.resource_uri = "file:/OTHER.H5".into();
    assert!(altered.original_entry_view().is_err());
}

#[test]
fn signed_catalogue_and_resource_receiver_budget_is_checked_before_streaming() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let raw = std::fs::read(f.dir.path().join("CATALOG.XML")).unwrap();
    let doc = xml(&raw).unwrap();
    let certificates = Certificates::parse(doc.root_element())
        .unwrap()
        .verified(&f.anchors, NOW)
        .unwrap();
    let data = b"authentic bathymetry bytes\x00\xff";
    let make = || {
        vec![Signature {
            id: "DATA".into(),
            certificate: "LEAF".into(),
            bytes: signature(&f.key, data),
            target: None,
        }]
    };
    let path = f.dir.path().join("DATA.H5");
    assert!(verify_resource_with_limit(path.clone(), make(), &certificates, Some(27)).is_err());
    assert_eq!(
        verify_resource_with_limit(path, make(), &certificates, Some(28))
            .unwrap()
            .size,
        28
    );
    let excessive = vec![b' '; MAX_XML as usize + 1];
    std::fs::write(f.dir.path().join("CATALOG.XML"), excessive).unwrap();
    assert!(verify_exchange_catalogue(f.dir.path(), &f.anchors, NOW).is_err());
}

fn copied_signature_notice(
    f: &Fixture,
    xml: &str,
) -> (tempfile::TempDir, AuthenticatedExchangeCatalogue) {
    f.catalogue(xml.as_bytes());
    let dir = tempfile::tempdir().unwrap();
    for name in ["CATALOG.XML", "CATALOG.SIGN"] {
        std::fs::copy(f.dir.path().join(name), dir.path().join(name)).unwrap();
    }
    let catalogue = verify_exchange_catalogue(dir.path(), &f.anchors, NOW).unwrap();
    (dir, catalogue)
}

#[test]
fn copied_original_signatures_pin_original_bytes_and_incoming_catalogue() {
    let f = Fixture::new(true, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let original = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML"))
        .unwrap()
        .replace("newDataset", "cancellation");
    let (dir, incoming) = copied_signature_notice(&f, &xml);
    assert!(!dir.path().join("DATA.H5").exists());
    let binding = original.bind_copied_signatures(&incoming).unwrap();
    assert_eq!(
        binding.original().resource_sha384(),
        original.resource_sha384()
    );
    assert_eq!(
        binding.incoming_catalogue().catalogue_sha384(),
        incoming.catalogue_sha384()
    );
    assert_eq!(binding.original().signatures().len(), 2);
    std::fs::write(f.dir.path().join("DATA.H5"), b"replacement").unwrap();
    assert_eq!(
        std::fs::read(binding.resource_snapshot().path()).unwrap(),
        b"authentic bathymetry bytes\x00\xff"
    );
    assert!(original.bind_copied_signatures(&incoming).is_err());
}

#[test]
fn trusted_notice_with_same_signature_ids_cannot_replace_original_der() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let original = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    let old_der = STANDARD.encode(original.signatures()[0].der());
    let other_der = STANDARD.encode(signature(&f.key, b"different resource"));
    let bad = xml.replace(&old_der, &other_der);
    let (_dir, incoming) = copied_signature_notice(&f, &bad);
    assert!(original
        .bind_copied_signatures(&incoming)
        .unwrap_err()
        .to_string()
        .contains("Copied signature value"));
}

#[test]
fn copied_original_graph_status_algorithm_and_exact_uri_are_bound() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let original = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    for bad in [
        xml.replace("signatureRef=\"DATA\"", "signatureRef=\"CHAIN\""),
        xml.replace("dataStatus=\"unencrypted\"", "dataStatus=\"encrypted\""),
        xml.replace("ECDSA-384-SHA2", "8"),
        xml.replace("file:/DATA.H5", "file:/OTHER.H5"),
        xml.replace("id=\"CHAIN\"", "id=\"DATA\""),
    ] {
        let (_dir, incoming) = copied_signature_notice(&f, &bad);
        assert!(original.bind_copied_signatures(&incoming).is_err());
    }
}

#[test]
fn copied_certificate_must_match_original_der_not_only_id_or_public_key() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let original = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    let cert_der = original.signatures()[0].signer_certificate_der();
    // Same signing key, different certificate subject: identity must reject.
    let another = certificate("ANOTHER IDENTITY", &f.key, None, false, true)
        .to_der()
        .unwrap();
    for der in [another, [cert_der, &[0u8][..]].concat()] {
        let bad = xml.replace(&STANDARD.encode(cert_der), &STANDARD.encode(der));
        let (_dir, incoming) = copied_signature_notice(&f, &bad);
        assert!(original.bind_copied_signatures(&incoming).is_err());
    }
}

#[test]
fn copied_signature_envelopes_keep_expanded_names_but_ignore_base64_whitespace() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let original = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    let encoded = STANDARD.encode(original.signatures()[0].der());
    let whitespace = encoded
        .chars()
        .collect::<Vec<_>>()
        .chunks(12)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n ");
    let (_dir, incoming) = copied_signature_notice(&f, &xml.replace(&encoded, &whitespace));
    original.bind_copied_signatures(&incoming).unwrap();
    let bad = xml.replace(SE, "urn:spoof");
    let (_dir, incoming) = copied_signature_notice(&f, &bad);
    assert!(original.bind_copied_signatures(&incoming).is_err());
}

#[test]
fn copied_signatures_reuse_parser_snapshot_after_live_original_is_removed() {
    let f = Fixture::new(true, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let original = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let snapshot = std::sync::Arc::new(original.capture_resource().unwrap());
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    let (_dir, incoming) = copied_signature_notice(&f, &xml.replace("newDataset", "cancellation"));
    std::fs::remove_file(original.resource_path()).unwrap();
    assert!(original.bind_copied_signatures(&incoming).is_err());
    let binding = original
        .bind_copied_signatures_from_snapshot(&incoming, snapshot.clone())
        .unwrap();
    assert!(std::ptr::eq(binding.resource_snapshot(), snapshot.as_ref()));
    assert_eq!(std::sync::Arc::strong_count(&snapshot), 2);
    drop(snapshot);
    assert_eq!(
        std::fs::read(binding.resource_snapshot().path()).unwrap(),
        b"authentic bathymetry bytes\x00\xff"
    );
}

#[test]
fn copied_snapshot_binding_rehashes_same_size_mutation_truncation_and_growth() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let original = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    let (_dir, incoming) = copied_signature_notice(&f, &xml);
    for bytes in [
        vec![b'x'; original.resource_size() as usize],
        vec![b'x'; 1],
        vec![b'x'; original.resource_size() as usize + 1],
    ] {
        let snapshot = std::sync::Arc::new(original.capture_resource().unwrap());
        std::fs::write(snapshot.path(), bytes).unwrap();
        assert!(original
            .bind_copied_signatures_from_snapshot(&incoming, snapshot)
            .is_err());
    }
}

#[test]
fn copied_snapshot_source_label_cannot_redirect_authentication_and_der_still_binds() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let report = f.verify(NOW).unwrap();
    let original = report
        .dataset_discovery
        .values()
        .next()
        .unwrap()
        .original_authentication();
    let xml = std::fs::read_to_string(f.dir.path().join("CATALOG.XML")).unwrap();
    let mut snapshot = original.capture_resource().unwrap();
    snapshot.source = f.dir.path().join("NONEXISTENT_DECOY.H5");
    let snapshot = std::sync::Arc::new(snapshot);
    let (_dir, incoming) = copied_signature_notice(&f, &xml);
    original
        .bind_copied_signatures_from_snapshot(&incoming, snapshot.clone())
        .unwrap();
    let old_der = STANDARD.encode(original.signatures()[0].der());
    let bad = xml.replace(
        &old_der,
        &STANDARD.encode(signature(&f.key, b"another payload")),
    );
    let (_otherdir, changed) = copied_signature_notice(&f, &bad);
    assert!(original
        .bind_copied_signatures_from_snapshot(&changed, snapshot)
        .is_err());
}

#[test]
fn captured_incoming_namespace_survives_source_deletion_without_authenticating_payloads() {
    let owned = {
        let f = Fixture::new(true, true, Nid::SECP384R1);
        f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
        let owned = capture_catalogue_authenticated_incoming_exchange(
            f.dir.path(),
            &f.anchors,
            NOW,
            IncomingExchangeLimits::default(),
        )
        .unwrap();
        assert_eq!(owned.file_count(), 3);
        let original = owned
            .resource_bytes("file:/DATA.H5")
            .unwrap()
            .unwrap()
            .to_vec();
        assert_eq!(original, b"authentic bathymetry bytes\x00\xff");
        assert!(owned.prove_ascii_resource_absent("DATA.H5").is_err());
        std::fs::write(f.dir.path().join("DATA.H5"), b"tampered source").unwrap();
        std::fs::write(f.dir.path().join("LATER.H5"), b"later arrival").unwrap();
        assert_eq!(owned.resource_bytes("DATA.H5").unwrap().unwrap(), original);
        assert!(owned.prove_ascii_resource_absent("LATER.H5").is_ok());
        owned
    }; // Entire original namespace, signing keys and fixture files are deleted.
    assert_eq!(
        owned.resource_bytes("DATA.H5").unwrap().unwrap(),
        b"authentic bathymetry bytes\x00\xff"
    );
    assert!(owned
        .catalogue()
        .discovery_view()
        .unwrap()
        .dataset_entries()
        .next()
        .is_some());
    assert_eq!(owned.namespace_sha384().len(), 96);
    assert!(!format!("{owned:?}").contains("bathymetry bytes"));
}

#[test]
fn captured_catalogue_only_missing_file_is_absence_not_resource_authentication() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    assert!(f.verify(NOW).is_ok());
    std::fs::remove_file(f.dir.path().join("DATA.H5")).unwrap();
    let owned = capture_catalogue_authenticated_incoming_exchange(
        f.dir.path(),
        &f.anchors,
        NOW,
        IncomingExchangeLimits::default(),
    )
    .unwrap();
    assert_eq!(owned.file_count(), 2);
    assert!(owned.resource_bytes("DATA.H5").unwrap().is_none());
    let absent = owned.prove_ascii_resource_absent("file:/DATA.H5").unwrap();
    assert_eq!(absent.relative_name(), "DATA.H5");
    assert!(std::ptr::eq(absent.incoming_exchange(), &owned));
    // The captured announcement is not a VerifiedResource or a cancellation.
    assert!(f.verify(NOW).is_err());
    let bytes = std::fs::read(f.dir.path().join("CATALOG.XML")).unwrap();
    std::fs::write(
        f.dir.path().join("CATALOG.XML"),
        [bytes, b" ".to_vec()].concat(),
    )
    .unwrap();
    assert!(capture_catalogue_authenticated_incoming_exchange(
        f.dir.path(),
        &f.anchors,
        NOW,
        IncomingExchangeLimits::default()
    )
    .is_err());
}

#[test]
fn incoming_receiver_limits_are_enforced_before_admission() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("DATA.H5", false, "LEAF", "unencrypted");
    let cap = IncomingExchangeLimits::default();
    for limits in [
        IncomingExchangeLimits {
            file_bytes: 1,
            ..cap
        },
        IncomingExchangeLimits {
            total_bytes: 1,
            ..cap
        },
        IncomingExchangeLimits { nodes: 1, ..cap },
        IncomingExchangeLimits {
            path_bytes: 1,
            ..cap
        },
        IncomingExchangeLimits {
            file_bytes: cap.file_bytes + 1,
            ..cap
        },
        IncomingExchangeLimits { nodes: 0, ..cap },
    ] {
        assert!(capture_catalogue_authenticated_incoming_exchange(
            f.dir.path(),
            &f.anchors,
            NOW,
            limits
        )
        .is_err());
    }
    // Sparse oversized file: decline by metadata before reading/allocating it.
    std::fs::File::create(f.dir.path().join("OVERSIZED.H5"))
        .unwrap()
        .set_len(cap.file_bytes + 1)
        .unwrap();
    assert!(
        capture_catalogue_authenticated_incoming_exchange(f.dir.path(), &f.anchors, NOW, cap)
            .is_err()
    );
}

#[test]
fn incoming_absence_rejects_case_percent_directory_and_portable_aliases() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("DATA.H5", false, "LEAF", "unencrypted");
    std::fs::write(f.dir.path().join("Other.H5."), b"alias").unwrap();
    std::fs::create_dir(f.dir.path().join("DIRECTORY.H5")).unwrap();
    let owned = capture_catalogue_authenticated_incoming_exchange(
        f.dir.path(),
        &f.anchors,
        NOW,
        IncomingExchangeLimits::default(),
    )
    .unwrap();
    for uri in [
        "data.h5",
        "%44ATA.H5",
        "OTHER.H5",
        "directory.h5",
        "file:/DATA.H5",
    ] {
        assert!(owned.prove_ascii_resource_absent(uri).is_err(), "{uri}");
    }
    for uri in [
        "../NONE.H5",
        "file://NONE.H5",
        "https://example.com/NONE.H5",
        "NONE/../DATA.H5",
    ] {
        assert!(owned.prove_ascii_resource_absent(uri).is_err(), "{uri}");
    }
}

#[test]
fn incoming_preserves_utf8_resources_without_general_unicode_absence_claim() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("DATA.H5", false, "LEAF", "unencrypted");
    std::fs::create_dir(f.dir.path().join("설명")).unwrap();
    std::fs::write(f.dir.path().join("설명/항만.txt"), "원본".as_bytes()).unwrap();
    let owned = capture_catalogue_authenticated_incoming_exchange(
        f.dir.path(),
        &f.anchors,
        NOW,
        IncomingExchangeLimits::default(),
    )
    .unwrap();
    // Use the actual platform spelling; APFS may return decomposed Unicode names.
    let entry = std::fs::read_dir(f.dir.path())
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.file_type().unwrap().is_dir())
        .unwrap();
    let file = std::fs::read_dir(entry.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let rel = file.strip_prefix(f.dir.path()).unwrap().to_str().unwrap();
    assert_eq!(
        owned.resource_bytes(rel).unwrap().unwrap(),
        "원본".as_bytes()
    );
    assert!(owned.prove_ascii_resource_absent("다른파일.H5").is_err());
}

#[cfg(unix)]
#[test]
fn incoming_rejects_symlink_files_directories_and_roots() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("DATA.H5", false, "LEAF", "unencrypted");
    std::os::unix::fs::symlink("DATA.H5", f.dir.path().join("LINK.H5")).unwrap();
    assert!(capture_catalogue_authenticated_incoming_exchange(
        f.dir.path(),
        &f.anchors,
        NOW,
        IncomingExchangeLimits::default()
    )
    .is_err());
    std::fs::remove_file(f.dir.path().join("LINK.H5")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), f.dir.path().join("LINKDIR")).unwrap();
    assert!(capture_catalogue_authenticated_incoming_exchange(
        f.dir.path(),
        &f.anchors,
        NOW,
        IncomingExchangeLimits::default()
    )
    .is_err());
    std::fs::remove_file(f.dir.path().join("LINKDIR")).unwrap();
    std::os::unix::fs::symlink(f.dir.path(), outside.path().join("ROOTLINK")).unwrap();
    assert!(capture_catalogue_authenticated_incoming_exchange(
        outside.path().join("ROOTLINK"),
        &f.anchors,
        NOW,
        IncomingExchangeLimits::default()
    )
    .is_err());
}

#[path = "trusted_cancellation_authority_tests.rs"]
mod trusted_cancellation_authority_tests;
