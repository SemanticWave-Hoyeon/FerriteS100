//! Authorize a catalogue by authenticated discovery metadata and immutable bytes.
use super::*;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CatalogueScope {
    Feature,
    Portrayal,
    Interoperability,
}
impl CatalogueScope {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "featureCatalogue" | "1" => Ok(Self::Feature),
            "portrayalCatalogue" | "2" => Ok(Self::Portrayal),
            "interoperabilityCatalogue" | "3" => Ok(Self::Interoperability),
            _ => bail!("Unknown catalogue scope {s:?}"),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueDiscovery {
    pub scope: CatalogueScope,
    pub edition_number: u32,
    pub version_number: String,
    pub issue_date: String,
    pub product_identifier: String,
    pub product_number: u32,
    pub specification_version: Option<String>,
}
pub struct AuthorizedCatalogue {
    pub metadata: CatalogueDiscovery,
    pub snapshot: AuthenticatedSnapshot,
    pub resource: VerifiedResource,
    pub trust_anchor_sha256: HashMap<String, String>,
    pub metadata_warnings: Vec<String>,
    pub revocation_checked: bool,
}
fn integer(n: Node<'_, '_>, name: &str) -> Result<u32> {
    let v: u32 = text(unique_child(n, XC, name)?)?
        .parse()
        .with_context(|| format!("Invalid {name}"))?;
    ensure!(v > 0, "{name} must be positive");
    Ok(v)
}
fn optional_text(n: Node<'_, '_>, name: &str) -> Result<Option<String>> {
    let mut nodes = n.children().filter(|c| is(*c, XC, name));
    let value = nodes.next().map(text).transpose()?.map(str::to_owned);
    ensure!(nodes.next().is_none(), "Duplicate {name}");
    Ok(value)
}
/// Only uncompressed, non-cancelled catalogues of the requested scope are ready to read.
/// Trust anchors are supplied independently; revocation remains explicitly unverified.
pub fn authorize_catalogue(
    path: impl AsRef<Path>,
    scope: CatalogueScope,
    anchors: &TrustAnchors,
    time: i64,
) -> Result<AuthorizedCatalogue> {
    let path = path.as_ref().canonicalize()?;
    let root = path
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .find(|p| p.join("CATALOG.SIGN").exists())
        .context("Catalogue requires a signed exchange set")?;
    let report = verify_exchange(root, anchors, time)?;
    let resource = report
        .resources
        .into_iter()
        .find(|r| r.path == path)
        .context("Requested catalogue is not signed by its exchange catalogue")?;
    let catalogue_snapshot = report.catalogue.snapshot()?;
    let bytes = read_xml(catalogue_snapshot.path())?;
    let document = xml(&bytes)?;
    let node = document.root_element();
    let mut matches = Vec::new();
    for container in node
        .children()
        .filter(|n| is(*n, XC, "catalogueDiscoveryMetadata"))
    {
        for entry in container
            .children()
            .filter(|n| is(*n, XC, "S100_CatalogueDiscoveryMetadata"))
        {
            let candidate = resource_path(root, text(unique_child(entry, XC, "fileName")?)?)?;
            if candidate == path {
                matches.push(entry);
            }
        }
    }
    ensure!(
        matches.len() == 1,
        "Requested file must have exactly one catalogue discovery record"
    );
    let entry = matches[0];
    let actual_scope = CatalogueScope::parse(text(unique_child(entry, XC, "scope")?)?)?;
    ensure!(
        actual_scope == scope,
        "Catalogue scope does not match requested {:?}",
        scope
    );
    if let Some(purpose) = optional_text(entry, "purpose")? {
        ensure!(
            purpose == "2" || purpose == "newEdition",
            "Catalogue is cancelled or has unsupported purpose: {purpose}"
        );
    }
    let compression = text(unique_child(entry, XC, "compressionFlag")?)?;
    ensure!(
        compression == "false" || compression == "0",
        "Compressed catalogue requires a bounded decoder before activation"
    );
    let edition_number = integer(entry, "editionNumber")?;
    let version_number = text(unique_child(entry, XC, "versionNumber")?)?.to_owned();
    let issue_date = text(unique_child(entry, XC, "issueDate")?)?.to_owned();
    let date = chrono::NaiveDate::parse_from_str(&issue_date, "%Y-%m-%d")
        .context("Invalid catalogue issueDate")?;
    ensure!(
        date.format("%Y-%m-%d").to_string() == issue_date,
        "Catalogue issueDate must be canonical YYYY-MM-DD"
    );
    let spec = unique_child(
        unique_child(entry, XC, "productSpecification")?,
        XC,
        "S100_ProductSpecification",
    )?;
    let product_identifier = text(unique_child(spec, XC, "productIdentifier")?)?.to_owned();
    let product_number = integer(spec, "number")?;
    let specification_version = optional_text(spec, "version")?;
    let snapshot = resource.snapshot()?;
    Ok(AuthorizedCatalogue {
        metadata: CatalogueDiscovery {
            scope: actual_scope,
            edition_number,
            version_number,
            issue_date,
            product_identifier,
            product_number,
            specification_version,
        },
        snapshot,
        resource,
        trust_anchor_sha256: report.trust_anchor_sha256,
        metadata_warnings: report.metadata_warnings,
        revocation_checked: report.revocation_checked,
    })
}
