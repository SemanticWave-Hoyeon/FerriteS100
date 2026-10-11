//! End-to-end S-98 activation: signed exchange catalogue -> authenticated
//! snapshot -> Part 16 reader -> S-98 profile. Locally generated keys only.
use base64::{engine::general_purpose::STANDARD, Engine};
use ferrite_interoperability::AuthenticatedCatalogue;
use ferrite_security::TrustAnchors;
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    sign::Signer,
    x509::{
        extension::{BasicConstraints, KeyUsage},
        X509NameBuilder, X509,
    },
};
use std::path::Path;

const NOW: i64 = 1_791_043_200;
const SE: &str = "http://www.iho.int/s100/se/5.2";
const XC: &str = "http://www.iho.int/s100/xc/5.2";
const S98_PRODUCTS: &str = "urn:mrn:iho:prod:s98:1:0:0:products";

fn key() -> PKey<Private> {
    PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::SECP384R1).unwrap()).unwrap())
        .unwrap()
}
fn certificate_at(
    name: &str,
    key: &PKey<Private>,
    issuer: Option<(&X509, &PKey<Private>)>,
    now: i64,
) -> X509 {
    let mut n = X509NameBuilder::new().unwrap();
    n.append_entry_by_text("CN", name).unwrap();
    let n = n.build();
    let ca = issuer.is_none();
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_serial_number(
        &BigNum::from_u32(if ca { 1 } else { 2 })
            .unwrap()
            .to_asn1_integer()
            .unwrap(),
    )
    .unwrap();
    b.set_subject_name(&n).unwrap();
    b.set_issuer_name(issuer.map(|i| i.0.subject_name()).unwrap_or(&n))
        .unwrap();
    b.set_pubkey(key).unwrap();
    b.set_not_before(&Asn1Time::from_unix(now - 3600).unwrap())
        .unwrap();
    b.set_not_after(&Asn1Time::from_unix(now + 86400).unwrap())
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
    } else {
        usage.digital_signature();
    }
    b.append_extension(usage.build().unwrap()).unwrap();
    b.sign(issuer.map(|i| i.1).unwrap_or(key), MessageDigest::sha384())
        .unwrap();
    b.build()
}
fn sign(key: &PKey<Private>, bytes: &[u8]) -> String {
    let mut s = Signer::new(MessageDigest::sha384(), key).unwrap();
    s.update(bytes).unwrap();
    STANDARD.encode(s.sign_to_vec().unwrap())
}

/// Writes a signed exchange set holding `ic` and returns its path and anchors.
fn signed_exchange(dir: &Path, ic: &str, version: &str) -> (std::path::PathBuf, TrustAnchors) {
    let (path, root) = signed_exchange_at(dir, ic, version, NOW);
    let mut anchors = TrustAnchors::default();
    anchors.install_pem("IHO", &root.to_pem().unwrap()).unwrap();
    (path, anchors)
}
fn signed_exchange_at(dir: &Path, ic: &str, version: &str, now: i64) -> (std::path::PathBuf, X509) {
    let root_key = key();
    let root = certificate_at("ROOT", &root_key, None, now);
    let leaf_key = key();
    let leaf = certificate_at("PRODUCER", &leaf_key, Some((&root, &root_key)), now);
    let container = format!(
        "<se:schemeAdministrator id=\"IHO\"/><se:certificate id=\"LEAF\" issuer=\"IHO\">{}</se:certificate>",
        STANDARD.encode(leaf.to_der().unwrap())
    );
    let path = dir.join("098AA00010000.XML");
    std::fs::write(&path, ic).unwrap();
    let metadata = format!("<xc:S100_CatalogueDiscoveryMetadata><xc:fileName>file:/098AA00010000.XML</xc:fileName><xc:scope>interoperabilityCatalogue</xc:scope><xc:editionNumber>1</xc:editionNumber><xc:versionNumber>{version}</xc:versionNumber><xc:issueDate>2026-10-04</xc:issueDate><xc:compressionFlag>false</xc:compressionFlag><xc:productSpecification><xc:S100_ProductSpecification><xc:productIdentifier>S-98</xc:productIdentifier><xc:number>98</xc:number><xc:version>2.0.0</xc:version></xc:S100_ProductSpecification></xc:productSpecification><xc:digitalSignatureReference>ECDSA-384-SHA2</xc:digitalSignatureReference><xc:digitalSignatureValue><se:S100_SE_SignatureOnData id=\"IC\" certificateRef=\"LEAF\" dataStatus=\"unencrypted\">{}</se:S100_SE_SignatureOnData></xc:digitalSignatureValue></xc:S100_CatalogueDiscoveryMetadata>", sign(&leaf_key, ic.as_bytes()));
    let exchange = format!("<xc:S100_ExchangeCatalogue xmlns:xc=\"{XC}\" xmlns:se=\"{SE}\"><xc:certificates>{container}</xc:certificates><xc:catalogueDiscoveryMetadata>{metadata}</xc:catalogueDiscoveryMetadata></xc:S100_ExchangeCatalogue>");
    std::fs::write(dir.join("CATALOG.XML"), &exchange).unwrap();
    let signature = format!("<se:StandaloneDigitalSignature xmlns:se=\"{SE}\"><se:filename>CATALOG.XML</se:filename><se:certificates>{container}</se:certificates><se:digitalSignature id=\"CAT\" certificateRef=\"LEAF\">{}</se:digitalSignature></se:StandaloneDigitalSignature>", sign(&leaf_key, exchange.as_bytes()));
    std::fs::write(dir.join("CATALOG.SIGN"), signature).unwrap();
    (path, root)
}

fn s98_ic() -> String {
    include_str!("display-plane.xml")
        .replace("urn:ferrite:test:products", S98_PRODUCTS)
        .replace(
            "<description>Bathymetry plane</description>",
            "<description>Bathymetry plane</description><interoperabilityLevel>1</interoperabilityLevel>",
        )
        .replace(
            "<order>-10</order>",
            "<order>-10</order><interoperabilityLevel>1</interoperabilityLevel>",
        )
}

#[test]
fn signed_s98_catalogue_activates_and_profile_violations_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (path, anchors) = signed_exchange(dir.path(), &s98_ic(), "1.0.0");
    let ic = AuthenticatedCatalogue::load(&path, &anchors, NOW).unwrap();
    assert_eq!(ic.catalogue.level, Some(1));
    assert!(ic.profile_findings.is_empty());

    // The Annex A dictionary MRN activates with an explicit finding.
    let dir = tempfile::tempdir().unwrap();
    let annex = s98_ic().replace(S98_PRODUCTS, "urn:mrn:iho:prod:s98:1:1:0:products");
    let (path, anchors) = signed_exchange(dir.path(), &annex, "1.0.0");
    let ic = AuthenticatedCatalogue::load(&path, &anchors, NOW).unwrap();
    assert_eq!(ic.profile_findings.len(), 2);

    // Correctly signed but outside the S-98 profile: refused at activation.
    for (from, to) in [
        ("<requirementType>OEM", "<requirementType>vendor"),
        (S98_PRODUCTS, "urn:ferrite:test:products"),
        (
            "<order>-10</order><interoperabilityLevel>1</interoperabilityLevel>",
            "<order>-10</order>",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (path, anchors) = signed_exchange(dir.path(), &s98_ic().replace(from, to), "1.0.0");
        let error = AuthenticatedCatalogue::load(&path, &anchors, NOW)
            .err()
            .unwrap();
        assert!(!format!("{error:#}").is_empty(), "{from}");
    }

    // Tampering after signing is rejected before XML interpretation.
    let dir = tempfile::tempdir().unwrap();
    let (path, anchors) = signed_exchange(dir.path(), &s98_ic(), "1.0.0");
    std::fs::write(
        &path,
        s98_ic().replace("<drawingPriority>4", "<drawingPriority>5"),
    )
    .unwrap();
    assert!(AuthenticatedCatalogue::load(&path, &anchors, NOW).is_err());
}

/// Writes a locally signed S-98 Level 2 exchange set for manual application
/// checks: `FERRITE_IC_FIXTURE_DIR=<dir> cargo test ... -- --ignored`.
/// The combination suppresses S-102 BathymetryCoverage, replaced by S-101.
#[test]
#[ignore = "Writes a manual-validation fixture"]
fn write_level2_application_fixture() {
    let dir = std::path::PathBuf::from(std::env::var("FERRITE_IC_FIXTURE_DIR").unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    let product = format!("codeList=\"{S98_PRODUCTS}\"");
    let order = std::env::var("FERRITE_IC_FIXTURE_COVERAGE_ORDER").unwrap_or("-500".into());
    let ic = s98_ic()
        .replace("<order>-500</order>", &format!("<order>{order}</order>"))
        // The S-102 portrayal catalogue enables viewing group 13030.
        .replace(
            "<drawingPriority>3</drawingPriority><viewingGroup>11010</viewingGroup>",
            "<drawingPriority>3</drawingPriority><viewingGroup>13030</viewingGroup>",
        )
        .replace("<interoperabilityLevel>1</interoperabilityLevel><requirementType>", "<interoperabilityLevel>2</interoperabilityLevel><requirementType>")
        .replace(
            "<features/>",
            &format!("<features><S100_IC_Feature><identifier>depthAreaRule</identifier><featureCode>DepthArea</featureCode><product {product} codeListValue=\"S-101\">ENC</product><drawingPriority>1</drawingPriority><viewingGroup>13030</viewingGroup></S100_IC_Feature></features>"),
        )
        .replace(
            "</displayPlanes>",
            &format!("</displayPlanes><predefinedProductCombinations><S100_IC_PredefinedCombination><identifier>encOverBathymetry</identifier><name>ENC replaces bathymetry</name><description>Local validation only</description><useConditions>Test</useConditions><interoperabilityLevel>2</interoperabilityLevel><includedProduct {product} codeListValue=\"S-101\">ENC</includedProduct><includedProduct {product} codeListValue=\"S-102\">Bathymetry</includedProduct><suppressedFeatureLayers><S100_IC_SuppressedFeatureLayer><identifier>bathymetryLayer</identifier><featureCode>BathymetryCoverage</featureCode><product {product} codeListValue=\"S-102\">Bathymetry</product><featureRef>depthAreaRule</featureRef></S100_IC_SuppressedFeatureLayer></suppressedFeatureLayers></S100_IC_PredefinedCombination></predefinedProductCombinations>"),
        );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (path, root) = signed_exchange_at(&dir, &ic, "1.0.0", now);
    std::fs::write(dir.join("ROOT.pem"), root.to_pem().unwrap()).unwrap();
    let mut anchors = TrustAnchors::default();
    anchors.install_pem("IHO", &root.to_pem().unwrap()).unwrap();
    let loaded = AuthenticatedCatalogue::load(&path, &anchors, now).unwrap();
    assert_eq!(loaded.catalogue.combinations.len(), 1);
}
