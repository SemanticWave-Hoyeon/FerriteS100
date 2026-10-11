//! Owned metadata evidence for S-101 lifecycle decisions. OFF reads producer
//! metadata without any authenticity claim; it never constructs signed wrappers.
use crate::s101_xc_coverage::{self, XcDataCoverage};
use anyhow::{bail, ensure, Context, Result};
use chrono::NaiveDate;
use ferrite_security::{AuthorizedDatasets, DatasetDiscoveryAuthorization, DatasetPurpose};
use roxmltree::{Document, Node, ParsingOptions};
use sha2::{Digest, Sha256, Sha384};
use std::sync::Arc;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

const XC: &str = "http://www.iho.int/s100/xc/5.2";
const MAX_XML: u64 = 8 * 1024 * 1024;
const MAX_RESOURCE: u64 = 512 * 1024 * 1024;

#[derive(Clone)]
enum Provenance {
    AuthenticatedCatalogue,
    UnverifiedCatalogue,
}
#[derive(Clone)]
enum CatalogueHash {
    AuthenticatedSha384(String),
    UnverifiedSha256([u8; 32]),
}

#[derive(Clone)]
enum CapturedCoverageCatalogue {
    Authenticated(ferrite_security::AuthenticatedDatasetDiscovery),
    Unverified {
        canonical_path: PathBuf,
        bytes: Arc<[u8]>,
    },
}
impl CapturedCoverageCatalogue {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Authenticated(bound) => bound.original_authentication().catalogue_bytes(),
            Self::Unverified { bytes, .. } => bytes,
        }
    }
}

#[derive(Clone)]
pub(crate) struct MetadataEvidence {
    pub purpose: DatasetPurpose,
    pub edition: u32,
    pub update: Option<u32>,
    pub issue_date: NaiveDate,
    pub(crate) data_coverage: Vec<XcDataCoverage>,
    product_specification: Option<crate::s101_xc_consistency::XcProduct>,
    coverage_catalogue: Option<CapturedCoverageCatalogue>,
    original_canonical_key: PathBuf,
    raw_resource_sha256: [u8; 32],
    catalogue_xml_hash: CatalogueHash,
    provenance: Provenance,
}
impl MetadataEvidence {
    pub(crate) fn compatible_optional(a: Option<&Self>, b: Option<&Self>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a.same_operation(b),
            // Optional OFF metadata may be retained only with the entire input
            // it was captured from; it cannot confer authenticated provenance.
            (Some(metadata), None) | (None, Some(metadata)) => !metadata.is_authenticated(),
        }
    }
    /// Compare the operation, retaining one complete path-bound proof. Unverified
    /// catalogues may package the same operation differently. Authenticated
    /// copies require the same signed catalogue identity as well.
    pub(crate) fn same_operation(&self, other: &Self) -> bool {
        self.purpose == other.purpose
            && self.edition == other.edition
            && self.update == other.update
            && self.issue_date == other.issue_date
            && self.raw_resource_sha256 == other.raw_resource_sha256
            && match (&self.catalogue_xml_hash, &other.catalogue_xml_hash) {
                (CatalogueHash::AuthenticatedSha384(a), CatalogueHash::AuthenticatedSha384(b)) => {
                    self.is_authenticated() && other.is_authenticated() && a == b
                }
                (CatalogueHash::UnverifiedSha256(_), CatalogueHash::UnverifiedSha256(_)) => {
                    !self.is_authenticated() && !other.is_authenticated()
                }
                _ => false,
            }
    }
    /// `original` is the retained canonical authorization key, not a live path
    /// to reopen. A deleted/changed original or catalogue cannot change evidence.
    pub(crate) fn verify_resource(&self, original: &Path, retained_data: &Path) -> Result<()> {
        ensure!(
            original == self.original_canonical_key,
            "Lifecycle metadata original canonical key mismatch"
        );
        if let Some(capture) = &self.coverage_catalogue {
            match (capture, &self.catalogue_xml_hash) {
                (
                    CapturedCoverageCatalogue::Authenticated(bound),
                    CatalogueHash::AuthenticatedSha384(expected),
                ) => ensure!(
                    bound.catalogue_sha384() == expected,
                    "Retained XC coverage authentication mismatch"
                ),
                (
                    CapturedCoverageCatalogue::Unverified { canonical_path, .. },
                    CatalogueHash::UnverifiedSha256(expected),
                ) => {
                    ensure!(
                        canonical_path.is_absolute(),
                        "Retained XC path must be canonical absolute"
                    );
                    ensure!(
                        <[u8; 32]>::from(Sha256::digest(capture.bytes())) == *expected,
                        "Retained XC coverage bytes mismatch"
                    );
                }
                _ => bail!("Retained XC coverage provenance mismatch"),
            }
            for row in &self.data_coverage {
                ensure!(
                    capture.bytes().get(row.entry_range.clone()).is_some()
                        && capture
                            .bytes()
                            .get(row.bounding_polygon_range.clone())
                            .is_some(),
                    "Retained XC coverage range mismatch"
                );
            }
        }
        let (digest, _, _) = resource_digest(retained_data)?;
        ensure!(
            digest == self.raw_resource_sha256,
            "Retained dataset differs from lifecycle metadata evidence"
        );
        Ok(())
    }
    pub(crate) fn validate_xc_profile(
        &self,
        id: &ferrite_s100_core::DatasetIdentification,
    ) -> Result<()> {
        crate::s101_xc_consistency::validate_profile(
            self.product_specification.as_ref(),
            self.purpose,
            &self.data_coverage,
            id,
        )?;
        Ok(())
    }
    pub(crate) fn validate_effective_xc_scales(
        &self,
        cell: &ferrite_s100_core::S101Cell,
    ) -> Result<crate::s101_xc_consistency::ScaleConsistency> {
        crate::s101_xc_consistency::compare_effective(
            self.product_specification.as_ref(),
            self.purpose,
            &self.data_coverage,
            cell,
        )
    }
    pub(crate) fn validate_xc_regions(
        &self,
        cell: &ferrite_s100_core::S101Cell,
    ) -> Result<crate::s101_xc_region::RegionStatus> {
        let numeric = self.validate_effective_xc_scales(cell)?;
        if matches!(
            numeric,
            crate::s101_xc_consistency::ScaleConsistency::LegacyNotApplied
                | crate::s101_xc_consistency::ScaleConsistency::CancellationRequiresRetainedOriginal
                | crate::s101_xc_consistency::ScaleConsistency::NullInterpretationRequired
        ) {
            return Ok(crate::s101_xc_region::RegionStatus::Unverified);
        }
        let Some(capture) = &self.coverage_catalogue else {
            return Ok(crate::s101_xc_region::RegionStatus::Unverified);
        };
        crate::s101_xc_region::compare(capture.bytes(), &self.data_coverage, cell)
    }
    pub(crate) fn is_authenticated(&self) -> bool {
        matches!(self.provenance, Provenance::AuthenticatedCatalogue)
    }
}

fn resource_digest(path: &Path) -> Result<([u8; 32], [u8; 48], u64)> {
    let input = File::open(path)?;
    let metadata = input.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_RESOURCE,
        "Lifecycle dataset exceeds regular-file receiver budget"
    );
    let mut input = input.take(MAX_RESOURCE + 1);
    let mut sha256 = Sha256::new();
    let mut sha384 = Sha384::new();
    let mut bytes = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = input.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .context("Lifecycle dataset size overflow")?;
        ensure!(
            size <= MAX_RESOURCE,
            "Lifecycle dataset grew beyond receiver budget"
        );
        sha256.update(&bytes[..n]);
        sha384.update(&bytes[..n]);
    }
    Ok((sha256.finalize().into(), sha384.finalize().into(), size))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn date(value: &str) -> Result<NaiveDate> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .context("Invalid lifecycle catalogue issueDate")?;
    ensure!(
        value.len() == 10 && date.format("%Y-%m-%d").to_string() == value,
        "Lifecycle issueDate must be canonical YYYY-MM-DD"
    );
    Ok(date)
}
fn number(value: &str) -> Result<u32> {
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "Invalid lifecycle catalogue integer"
    );
    Ok(value.parse()?)
}
fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Result<Node<'a, 'input>> {
    let mut children = node.children().filter(|n| {
        n.is_element() && n.tag_name().namespace() == Some(XC) && n.tag_name().name() == name
    });
    let value = children
        .next()
        .with_context(|| format!("Missing lifecycle catalogue {name}"))?;
    ensure!(
        children.next().is_none(),
        "Duplicate lifecycle catalogue {name}"
    );
    Ok(value)
}
fn text<'a>(node: Node<'a, '_>) -> Result<&'a str> {
    ensure!(
        node.children().all(|n| n.is_text()),
        "Nested lifecycle metadata text"
    );
    node.text()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("Empty lifecycle metadata text")
}
fn purpose(value: &str) -> Result<DatasetPurpose> {
    match value {
        "1" | "newDataset" => Ok(DatasetPurpose::NewDataset),
        "2" | "newEdition" => Ok(DatasetPurpose::NewEdition),
        "3" | "update" => Ok(DatasetPurpose::Update),
        "4" | "reissue" => Ok(DatasetPurpose::Reissue),
        "5" | "cancellation" => Ok(DatasetPurpose::Cancellation),
        _ => bail!("Unsupported lifecycle dataset purpose"),
    }
}
fn resource_path(root: &Path, uri: &str) -> Result<PathBuf> {
    // Decode before traversal checks, including encoded separators and dots.
    let mut decoded = Vec::with_capacity(uri.len());
    let mut input = uri.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = input.next().context("Truncated URI escape")?;
            let low = input.next().context("Truncated URI escape")?;
            let digit = |v: u8| (v as char).to_digit(16).context("Invalid URI escape");
            decoded.push((digit(high)? * 16 + digit(low)?) as u8);
        } else {
            decoded.push(byte);
        }
    }
    let decoded = String::from_utf8(decoded)?.replace('\\', "/");
    let name = if let Some(name) = decoded.strip_prefix("file:/") {
        name
    } else {
        ensure!(
            !decoded.contains(':') && !decoded.starts_with('/'),
            "Unsupported lifecycle resource URI"
        );
        &decoded
    };
    ensure!(
        !name.is_empty()
            && !name.starts_with('/')
            && name.split('/').all(|part| !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(':')
                && !part.contains('\0')),
        "Unsafe lifecycle exchange-relative resource path"
    );
    let path = root
        .join(name)
        .canonicalize()
        .context("Missing lifecycle catalogue resource")?;
    ensure!(
        path.starts_with(root) && path.is_file(),
        "Lifecycle resource escapes exchange root"
    );
    Ok(path)
}

/// Select only the entry bound by the verifier, from its full namespace-owning
/// retained catalogue. A helper test is not construction of an authority token.
fn capture_authenticated_rows(
    bytes: &[u8],
    range: std::ops::Range<usize>,
    resource_uri: &str,
) -> Result<(
    Vec<XcDataCoverage>,
    Option<crate::s101_xc_consistency::XcProduct>,
)> {
    let xml = std::str::from_utf8(bytes)?;
    let document = Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 200_000,
        },
    )?;
    let entry = document
        .descendants()
        .find(|n| {
            n.is_element()
                && n.tag_name().namespace() == Some(XC)
                && n.tag_name().name() == "S100_DatasetDiscoveryMetadata"
                && n.range() == range
        })
        .context("Authenticated coverage discovery range mismatch")?;
    ensure!(
        text(child(entry, "fileName")?)? == resource_uri,
        "Authenticated coverage discovery logical URI mismatch"
    );
    Ok((
        s101_xc_coverage::capture(entry)?,
        crate::s101_xc_consistency::capture_product(entry)?,
    ))
}

pub(crate) fn capture(
    original: &Path,
    retained_data: &Path,
    authorization: &AuthorizedDatasets,
) -> Result<Option<MetadataEvidence>> {
    let key = original.canonicalize()?;
    let authorization = authorization.checked_dataset_discovery(&key)?;
    match authorization {
        DatasetDiscoveryAuthorization::Authenticated(bound) => {
            let (sha256, sha384, size) = resource_digest(retained_data)?;
            ensure!(
                bound.resource_path() == key
                    && bound.resource_size() == size
                    && bound.resource_sha384() == hex(&sha384),
                "Lifecycle input differs from authenticated discovery resource"
            );
            let proof = bound.original_authentication();
            let (data_coverage, product_specification) = capture_authenticated_rows(
                proof.catalogue_bytes(),
                proof.discovery_range(),
                proof.resource_uri(),
            )?;
            let discovery = bound.discovery();
            ensure!(
                discovery.edition_number > 0 && discovery.update_number.is_none_or(|n| n <= 999),
                "Lifecycle catalogue edition/update range is invalid"
            );
            Ok(Some(MetadataEvidence {
                purpose: discovery.purpose,
                edition: discovery.edition_number,
                update: discovery.update_number,
                issue_date: date(discovery.issue_date.as_str())?,
                original_canonical_key: key,
                raw_resource_sha256: sha256,
                catalogue_xml_hash: CatalogueHash::AuthenticatedSha384(
                    bound.catalogue_sha384().to_owned(),
                ),
                data_coverage,
                product_specification,
                coverage_catalogue: Some(CapturedCoverageCatalogue::Authenticated(bound.clone())),
                provenance: Provenance::AuthenticatedCatalogue,
            }))
        }
        DatasetDiscoveryAuthorization::SignatureVerificationDisabled
        | DatasetDiscoveryAuthorization::UnsignedEvaluation => {
            let Some(root) = original
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .find(|p| p.join("CATALOG.XML").exists())
            else {
                return Ok(None);
            };
            let root = root.canonicalize()?;
            let xml_path = root.join("CATALOG.XML").canonicalize()?;
            ensure!(
                xml_path.starts_with(&root),
                "Lifecycle catalogue symlink escapes exchange root"
            );
            let file = File::open(&xml_path)?;
            ensure!(
                file.metadata()?.is_file() && file.metadata()?.len() <= MAX_XML,
                "Lifecycle catalogue exceeds XML receiver budget"
            );
            let mut bytes = Vec::new();
            file.take(MAX_XML + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_XML,
                "Lifecycle catalogue grew beyond XML receiver budget"
            );
            let xml = std::str::from_utf8(&bytes)?;
            let document = Document::parse_with_options(
                xml,
                ParsingOptions {
                    allow_dtd: false,
                    nodes_limit: 200_000,
                },
            )?;
            let catalogue = document.root_element();
            ensure!(
                catalogue.tag_name().namespace() == Some(XC)
                    && catalogue.tag_name().name() == "S100_ExchangeCatalogue",
                "Unsupported lifecycle catalogue namespace/type"
            );
            let mut matched = None;
            for container in catalogue.children().filter(|n| {
                n.is_element()
                    && n.tag_name().namespace() == Some(XC)
                    && n.tag_name().name() == "datasetDiscoveryMetadata"
            }) {
                for entry in container.children().filter(|n| {
                    n.is_element()
                        && n.tag_name().namespace() == Some(XC)
                        && n.tag_name().name() == "S100_DatasetDiscoveryMetadata"
                }) {
                    if resource_path(&root, text(child(entry, "fileName")?)?)? == key {
                        ensure!(
                            matched.replace(entry).is_none(),
                            "Ambiguous lifecycle dataset discovery"
                        );
                    }
                }
            }
            let entry = matched.context("Dataset missing from lifecycle catalogue")?;
            let purpose = purpose(text(child(entry, "purpose")?)?)?;
            let edition = number(text(child(entry, "editionNumber")?)?)?;
            ensure!(
                edition > 0,
                "Lifecycle catalogue target edition must be positive"
            );
            let mut updates = entry.children().filter(|n| {
                n.is_element()
                    && n.tag_name().namespace() == Some(XC)
                    && n.tag_name().name() == "updateNumber"
            });
            let update = updates.next().map(|n| number(text(n)?)).transpose()?;
            ensure!(
                updates.next().is_none() && update.is_none_or(|n| n <= 999),
                "Lifecycle catalogue update is duplicate or exceeds999"
            );
            let issue_date = date(text(child(entry, "issueDate")?)?)?;
            let (raw_resource_sha256, _, _) = resource_digest(retained_data)?;
            let evidence = MetadataEvidence {
                purpose,
                edition,
                update,
                issue_date,
                original_canonical_key: key,
                raw_resource_sha256,
                catalogue_xml_hash: CatalogueHash::UnverifiedSha256(Sha256::digest(&bytes).into()),
                data_coverage: s101_xc_coverage::capture(entry)?,
                product_specification: crate::s101_xc_consistency::capture_product(entry)?,
                coverage_catalogue: Some(CapturedCoverageCatalogue::Unverified {
                    canonical_path: xml_path.clone(),
                    bytes: crate::s101_xc_capture_pool::retain_unverified(&xml_path, &bytes)?,
                }),
                provenance: Provenance::UnverifiedCatalogue,
            };
            // Retain and report the exact OFF catalogue identity without treating
            // its digest as authentication or requiring identical XML packaging.
            if let CatalogueHash::UnverifiedSha256(digest) = &evidence.catalogue_xml_hash {
                tracing::debug!(
                    "Captured unverified lifecycle catalogue SHA256={:02x?}; no authentication claim",
                    digest
                );
            }
            Ok(Some(evidence))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_security::UnauthenticatedSnapshot;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Fixture {
        temp: PathBuf,
        root: PathBuf,
        original: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let temp = std::env::temp_dir().join(format!(
                "ferrite-lifecycle-metadata-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let root = temp.join("S100_ROOT");
            std::fs::create_dir_all(&root).unwrap();
            // Runtime passes retained canonical keys. macOS temp_dir() may
            // spell /var while canonicalize() spells /private/var.
            let root = root.canonicalize().unwrap();
            let original = root.join("101AA00TEST.000");
            std::fs::write(&original, b"retained input A").unwrap();
            Self {
                temp,
                root,
                original,
            }
        }
        fn xml(&self, body: &str) {
            std::fs::write(self.root.join("CATALOG.XML"), format!("<xc:S100_ExchangeCatalogue xmlns:xc=\"{XC}\"><xc:datasetDiscoveryMetadata>{body}</xc:datasetDiscoveryMetadata></xc:S100_ExchangeCatalogue>")).unwrap();
        }
        fn entry(&self) -> String {
            "<xc:S100_DatasetDiscoveryMetadata><xc:fileName>file:/101AA00TEST.000</xc:fileName><xc:purpose>cancellation</xc:purpose><xc:editionNumber>4</xc:editionNumber><xc:updateNumber>3</xc:updateNumber><xc:issueDate>2026-10-07</xc:issueDate></xc:S100_DatasetDiscoveryMetadata>".into()
        }
        fn authorization(&self) -> AuthorizedDatasets {
            crate::dataset_signature_policy::unchecked_datasets(&[self.original.clone()]).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.temp);
        }
    }

    #[test]
    fn operation_equivalence_preserves_lifecycle_and_authentication_boundaries() {
        let a = MetadataEvidence {
            purpose: DatasetPurpose::Update,
            edition: 1,
            update: Some(1),
            issue_date: NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            original_canonical_key: "/a/101TEST.001".into(),
            raw_resource_sha256: [1; 32],
            catalogue_xml_hash: CatalogueHash::UnverifiedSha256([2; 32]),
            data_coverage: Vec::new(),
            product_specification: None,
            coverage_catalogue: None,
            provenance: Provenance::UnverifiedCatalogue,
        };
        let mut b = a.clone();
        b.original_canonical_key = "/b/101TEST.001".into();
        b.catalogue_xml_hash = CatalogueHash::UnverifiedSha256([3; 32]);
        assert!(a.same_operation(&b));
        assert!(MetadataEvidence::compatible_optional(Some(&a), None));
        assert!(MetadataEvidence::compatible_optional(None, Some(&a)));
        b.issue_date = NaiveDate::from_ymd_opt(2026, 10, 8).unwrap();
        assert!(!a.same_operation(&b));
        b = a.clone();
        b.provenance = Provenance::AuthenticatedCatalogue;
        b.catalogue_xml_hash = CatalogueHash::AuthenticatedSha384("signed-a".into());
        assert!(!a.same_operation(&b));
        let mut c = b.clone();
        assert!(!MetadataEvidence::compatible_optional(Some(&b), None));
        assert!(!MetadataEvidence::compatible_optional(None, Some(&b)));
        c.original_canonical_key = "/c/101TEST.001".into();
        assert!(b.same_operation(&c));
        c.catalogue_xml_hash = CatalogueHash::AuthenticatedSha384("signed-b".into());
        assert!(!b.same_operation(&c));
    }
    #[test]
    fn off_captured_values_and_resource_survive_live_xml_change_without_trust_promotion() {
        let f = Fixture::new();
        f.xml(&f.entry());
        std::fs::write(
            f.root.join("CATALOG.SIGN"),
            b"invalid signature deliberately ignored in OFF",
        )
        .unwrap();
        let snapshot = UnauthenticatedSnapshot::copy_bounded(&f.original, MAX_RESOURCE).unwrap();
        let authorization = f.authorization();
        let evidence = capture(&f.original, snapshot.path(), &authorization)
            .unwrap()
            .unwrap();
        assert!(!evidence.is_authenticated());
        assert_eq!(evidence.purpose, DatasetPurpose::Cancellation);
        assert_eq!((evidence.edition, evidence.update), (4, Some(3)));
        assert_eq!(
            evidence.issue_date,
            NaiveDate::from_ymd_opt(2026, 10, 7).unwrap()
        );
        let CatalogueHash::UnverifiedSha256(digest) = &evidence.catalogue_xml_hash else {
            panic!("OFF evidence claimed signed catalogue")
        };
        assert_eq!(
            *digest,
            <[u8; 32]>::from(Sha256::digest(
                std::fs::read(f.root.join("CATALOG.XML")).unwrap()
            ))
        );
        std::fs::write(f.root.join("CATALOG.XML"), b"later malformed XML").unwrap();
        std::fs::write(&f.original, b"new live input B").unwrap();
        evidence
            .clone()
            .verify_resource(&f.original, snapshot.path())
            .unwrap();
        assert_eq!(evidence.edition, 4);
        assert!(evidence.verify_resource(&f.original, &f.original).is_err());
        assert!(evidence
            .verify_resource(&f.root.join("OTHER.000"), snapshot.path())
            .is_err());
        assert!(capture(&f.original, snapshot.path(), &authorization).is_err());
        std::fs::remove_file(f.root.join("CATALOG.XML")).unwrap();
        assert!(capture(&f.original, snapshot.path(), &authorization)
            .unwrap()
            .is_none());
        evidence
            .verify_resource(&f.original, snapshot.path())
            .unwrap();
        assert!(matches!(
            authorization
                .checked_dataset_discovery(&f.original)
                .unwrap(),
            DatasetDiscoveryAuthorization::SignatureVerificationDisabled
        ));
    }

    #[test]
    fn off_rejects_ambiguous_filename_encoded_traversal_namespace_and_symlink_escape() {
        let f = Fixture::new();
        let authorization = f.authorization();
        let entry = f.entry();
        f.xml(&(entry.clone() + &entry));
        assert!(capture(&f.original, &f.original, &authorization).is_err());
        std::fs::write(f.temp.join("outside.000"), b"outside root").unwrap();
        f.xml(&entry.replace("file:/101AA00TEST.000", "file:/%2e%2e/outside.000"));
        assert!(capture(&f.original, &f.original, &authorization).is_err());
        for uri in [
            "file://outside.000",
            "https://example.com/chart",
            "file:/bad%00name",
            "file:/bad%GG",
        ] {
            f.xml(&entry.replace("file:/101AA00TEST.000", uri));
            assert!(
                capture(&f.original, &f.original, &authorization).is_err(),
                "{uri}"
            );
        }
        f.xml(&entry);
        let xml = std::fs::read_to_string(f.root.join("CATALOG.XML"))
            .unwrap()
            .replace(XC, "http://example.com/not-s100");
        std::fs::write(f.root.join("CATALOG.XML"), xml).unwrap();
        assert!(capture(&f.original, &f.original, &authorization).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(f.temp.join("outside.000"), f.root.join("escape.000"))
                .unwrap();
            f.xml(&(entry.clone() + &entry.replace("file:/101AA00TEST.000", "file:/escape.000")));
            assert!(capture(&f.original, &f.original, &authorization).is_err());
        }
    }

    #[test]
    fn off_rejects_invalid_or_duplicate_typed_metadata_instead_of_date_fallback() {
        let f = Fixture::new();
        let authorization = f.authorization();
        for (old, new) in [
            (
                "<xc:purpose>cancellation</xc:purpose>",
                "<xc:purpose>unsupported</xc:purpose>",
            ),
            (
                "<xc:editionNumber>4</xc:editionNumber>",
                "<xc:editionNumber>0</xc:editionNumber>",
            ),
            (
                "<xc:editionNumber>4</xc:editionNumber>",
                "<xc:editionNumber>4294967296</xc:editionNumber>",
            ),
            (
                "<xc:updateNumber>3</xc:updateNumber>",
                "<xc:updateNumber>1000</xc:updateNumber>",
            ),
            (
                "<xc:updateNumber>3</xc:updateNumber>",
                "<xc:updateNumber>-1</xc:updateNumber>",
            ),
            (
                "<xc:updateNumber>3</xc:updateNumber>",
                "<xc:updateNumber>3</xc:updateNumber><xc:updateNumber>4</xc:updateNumber>",
            ),
            (
                "<xc:issueDate>2026-10-07</xc:issueDate>",
                "<xc:issueDate>2026-02-30</xc:issueDate>",
            ),
            (
                "<xc:issueDate>2026-10-07</xc:issueDate>",
                "<xc:issueDate>2026-1-07</xc:issueDate>",
            ),
            ("<xc:issueDate>2026-10-07</xc:issueDate>", ""),
            (
                "<xc:issueDate>2026-10-07</xc:issueDate>",
                "<xc:issueDate>2026-10-07</xc:issueDate><xc:issueDate>2026-10-08</xc:issueDate>",
            ),
        ] {
            f.xml(&f.entry().replace(old, new));
            assert!(
                capture(&f.original, &f.original, &authorization).is_err(),
                "{new}"
            );
        }
        // Generic optional counter is not fabricated; cancellation adapter must require it.
        f.xml(
            &f.entry()
                .replace("<xc:updateNumber>3</xc:updateNumber>", ""),
        );
        assert_eq!(
            capture(&f.original, &f.original, &authorization)
                .unwrap()
                .unwrap()
                .update,
            None
        );
    }

    #[test]
    fn off_metadata_resource_and_owned_copy_budgets_fail_closed() {
        let f = Fixture::new();
        let authorization = f.authorization();
        let xml = File::create(f.root.join("CATALOG.XML")).unwrap();
        xml.set_len(MAX_XML + 1).unwrap();
        assert!(capture(&f.original, &f.original, &authorization).is_err());
        f.xml(&f.entry());
        let data = File::create(&f.original).unwrap();
        data.set_len(MAX_RESOURCE + 1).unwrap();
        assert!(capture(&f.original, &f.original, &authorization).is_err());
        assert!(UnauthenticatedSnapshot::copy_bounded(&f.original, MAX_RESOURCE).is_err());
        std::fs::write(&f.original, b"four").unwrap();
        assert!(UnauthenticatedSnapshot::copy_bounded(&f.original, 3).is_err());
        let snapshot = UnauthenticatedSnapshot::copy_bounded(&f.original, 4).unwrap();
        assert_eq!(std::fs::read(snapshot.path()).unwrap(), b"four");
        assert!(UnauthenticatedSnapshot::copy_bounded(&f.original, u64::MAX).is_err());
    }
}

#[cfg(test)]
#[path = "s101_xc_authenticated_tests.rs"]
mod authenticated_xc_tests;
