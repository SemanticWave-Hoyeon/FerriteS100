//! Read IC rules only from an independently authenticated, immutable catalogue snapshot.
use anyhow::{ensure, Context, Result};
use ferrite_security::{authorize_catalogue, AuthorizedCatalogue, CatalogueScope, TrustAnchors};
use std::{io::Read, path::Path};
pub struct AuthenticatedCatalogue {
    pub catalogue: super::Catalogue,
    pub authorization: AuthorizedCatalogue,
}
impl AuthenticatedCatalogue {
    /// Authentication precedes XML interpretation. This is a supported Part 16 subset,
    /// not complete S-98 conformance or operational approval.
    pub fn load(path: impl AsRef<Path>, anchors: &TrustAnchors, time: i64) -> Result<Self> {
        let authorization =
            authorize_catalogue(path, CatalogueScope::Interoperability, anchors, time)?;
        ensure!(
            authorization.resource.size <= super::MAX_BYTES as u64,
            "IC exceeds byte limit"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(authorization.snapshot.path())?
            .take(super::MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= super::MAX_BYTES, "IC exceeds byte limit");
        let catalogue =
            super::Catalogue::parse(std::str::from_utf8(&bytes).context("IC is not UTF-8")?)?;
        ensure!(
            catalogue.version == authorization.metadata.version_number,
            "IC version differs from authenticated discovery metadata"
        );
        ensure!(
            authorization.metadata.product_identifier == "S-98"
                && authorization.metadata.product_number == 98,
            "IC discovery metadata must identify S-98 product 98"
        );
        let specification = authorization
            .metadata
            .specification_version
            .as_deref()
            .context("IC discovery metadata requires specification version before activation")?
            .parse::<ferrite_kernel::SpecificationVersion>()?;
        ensure!(
            specification == "2.0.0".parse()?,
            "Unsupported S-98 specification version before activation"
        );
        let date = ferrite_kernel::parse_viewing_date(&catalogue.version_date)?;
        let issued = ferrite_kernel::parse_viewing_date(&authorization.metadata.issue_date)?;
        ensure!(
            date <= issued,
            "IC versionDate is later than authenticated issueDate"
        );
        Ok(Self {
            catalogue,
            authorization,
        })
    }
}
