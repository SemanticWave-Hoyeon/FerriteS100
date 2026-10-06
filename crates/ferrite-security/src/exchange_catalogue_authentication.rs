//! Authenticate catalogue bytes without claiming to verify absent dataset bytes.
//! Product admission, frozen incoming-tree ownership and removal authority remain
//! separate requirements. This type is never a VerifiedResource for a missing file.
use super::*;
use std::sync::Arc;

pub struct AuthenticatedExchangeCatalogue {
    pub(super) catalogue: VerifiedResource,
    pub(super) bytes: Arc<[u8]>,
    pub(super) verified_unix_seconds: i64,
    pub(super) trust_anchor_sha256: Arc<HashMap<String, String>>,
    pub(super) legacy_namespace: bool,
}
impl std::fmt::Debug for AuthenticatedExchangeCatalogue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticatedExchangeCatalogue")
            .field("sha384", &self.catalogue.authentication.sha384)
            .field("verified_unix_seconds", &self.verified_unix_seconds)
            .finish_non_exhaustive()
    }
}
impl AuthenticatedExchangeCatalogue {
    pub fn catalogue_bytes(&self) -> &[u8] { &self.bytes }
    pub fn catalogue_sha384(&self) -> &str { &self.catalogue.authentication.sha384 }
    pub fn signatures(&self) -> &[VerifiedSignatureDescriptor] { &self.catalogue.authentication.signatures }
    pub fn verified_unix_seconds(&self) -> i64 { self.verified_unix_seconds }
    pub fn trust_anchor_sha256(&self) -> &HashMap<String, String> { &self.trust_anchor_sha256 }
    pub fn revocation_checked(&self) -> bool { false }
    pub fn discovery_view(&self) -> Result<CatalogueDiscoveryView<'_>> {
        CatalogueDiscoveryView::parse(&self.bytes)
    }
}

/// Direct catalogue entries identified in the complete authenticated document.
/// Read-only nodes retain inherited namespaces and their original byte ranges.
pub struct CatalogueDiscoveryView<'a> {
    document: Document<'a>,
    dataset_ids: Vec<roxmltree::NodeId>,
}
impl<'a> CatalogueDiscoveryView<'a> {
    pub(super) fn parse(bytes: &'a [u8]) -> Result<Self> {
        let document = xml(bytes)?;
        let root = document.root_element();
        ensure!(is(root, XC, "S100_ExchangeCatalogue"), "Unsupported catalogue namespace/type");
        let mut dataset_ids = Vec::new();
        let mut names = HashSet::new();
        for entry in root.descendants().filter(|n| n.is_element() && [
            "S100_DatasetDiscoveryMetadata", "S100_SupportFileDiscoveryMetadata",
            "S100_CatalogueDiscoveryMetadata",
        ].contains(&n.tag_name().name())) {
            let class = entry.tag_name().name();
            ensure!(entry.tag_name().namespace() == Some(XC), "Wrong discovery namespace");
            let container = entry.parent().context("Missing discovery container")?;
            let wrapper = match class {
                "S100_DatasetDiscoveryMetadata" => "datasetDiscoveryMetadata",
                "S100_SupportFileDiscoveryMetadata" => "supportFileDiscoveryMetadata",
                _ => "catalogueDiscoveryMetadata",
            };
            ensure!(is(container, XC, wrapper) && container.parent() == Some(root),
                "Discovery must be directly contained by its catalogue wrapper");
            let name = text(unique_child(entry, XC, "fileName")?)?;
            ensure!(names.insert(resource_relative_name(name)?), "Duplicate or aliased resource filename");
            if class == "S100_DatasetDiscoveryMetadata" { dataset_ids.push(entry.id()); }
        }
        Ok(Self { document, dataset_ids })
    }
    pub fn dataset_entries(&self) -> impl Iterator<Item = Node<'_, 'a>> {
        self.dataset_ids.iter().map(|id| self.document.get_node(*id).expect("Retained document node"))
    }
    pub(super) fn dataset_id_at(&self, range: &std::ops::Range<usize>, uri: &str) -> Result<roxmltree::NodeId> {
        let entry = self.dataset_entries().find(|n| n.range() == *range)
            .context("Original dataset discovery byte range not found")?;
        ensure!(text(unique_child(entry, XC, "fileName")?)? == uri,
            "Original discovery filename differs from retained resource URI");
        // Require the same generic mandatory fields admitted during import.
        dataset_discovery::parse(entry)?;
        Ok(entry.id())
    }
}

pub struct OriginalEntryView<'a> {
    pub(super) view: CatalogueDiscoveryView<'a>,
    pub(super) id: roxmltree::NodeId,
}
impl<'a> OriginalEntryView<'a> {
    pub fn entry(&self) -> Node<'_, 'a> {
        self.view.document.get_node(self.id).expect("Retained original document node")
    }
}

/// Verify the independently rooted CATALOG.SIGN and exact bounded CATALOG.XML.
/// The returned opaque proof authenticates only catalogue bytes. A signed notice
/// does not gain dataset authentication, absence assurance or removal permission.
pub fn verify_exchange_catalogue(root: impl AsRef<Path>, anchors: &TrustAnchors, time: i64)
    -> Result<AuthenticatedExchangeCatalogue> {
    let root = root.as_ref().canonicalize()?;
    let sign_bytes = read_xml(&resource_path(&root, "CATALOG.SIGN")?)?;
    let standalone = xml(&sign_bytes)?;
    let node = standalone.root_element();
    ensure!(is(node, SE, "StandaloneDigitalSignature"), "Unsupported standalone signature namespace/type");
    let filename = text(unique_child(node, SE, "filename")?)?;
    ensure!(filename == "CATALOG.XML", "Catalogue signature filename must be CATALOG.XML");
    let certs = Certificates::parse(node)?.verified(anchors, time)?;
    let signatures = node.children().filter(|n| is(*n, SE, "digitalSignature"))
        .map(parse_signature).collect::<Result<Vec<_>>>()?;
    let catalogue = verify_resource_with_limit(resource_path(&root, filename)?, signatures, &certs, Some(MAX_XML))?;
    let bytes = read_xml(&catalogue.path)?;
    ensure!(hex(&hash(MessageDigest::sha384(), &bytes)?) == catalogue.authentication.sha384,
        "Catalogue changed during verification");
    let bytes: Arc<[u8]> = bytes.into();
    // Validate discovery placement and logical identity, but do not verify or
    // resolve datasets here. A fileless product adapter still has work to do.
    CatalogueDiscoveryView::parse(&bytes)?;
    let trust_anchor_sha256 = Arc::new(anchors.roots.iter()
        .map(|(id, c)| Ok((id.clone(), hex(&c.digest(MessageDigest::sha256())?))))
        .collect::<Result<_>>()?);
    Ok(AuthenticatedExchangeCatalogue { catalogue, bytes, verified_unix_seconds: time,
        trust_anchor_sha256, legacy_namespace: node.tag_name().namespace() != Some(SE) })
}
