//! Dataset discovery is interpreted only from the catalogue bytes authenticated
//! by verify_exchange. It does not itself apply updates or certify product rules.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DatasetPurpose {
    NewDataset,
    NewEdition,
    Update,
    Reissue,
    Cancellation,
}
impl DatasetPurpose {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "1" | "newDataset" => Ok(Self::NewDataset),
            "2" | "newEdition" => Ok(Self::NewEdition),
            "3" | "update" => Ok(Self::Update),
            "4" | "reissue" => Ok(Self::Reissue),
            "5" | "cancellation" => Ok(Self::Cancellation),
            _ => bail!("Unsupported dataset purpose {value:?}"),
        }
    }
}

/// Validated ISO date, preserved exactly as delivered. Equality of dates is not
/// normalized away; product-specific sequencing belongs to the application.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DatasetIssueDate(String);
impl DatasetIssueDate {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    fn parse(value: &str) -> Result<Self> {
        let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .context("Invalid dataset issueDate")?;
        ensure!(
            value.len() == 10 && date.format("%Y-%m-%d").to_string() == value,
            "Dataset issueDate must be canonical YYYY-MM-DD"
        );
        Ok(Self(value.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatasetDiscovery {
    pub purpose: DatasetPurpose,
    /// Positive catalogue edition being cancelled, not physical DSID DSED=0.
    pub edition_number: u32,
    /// Optional in generic S-100. S-101 consumers must require it and enforce
    /// their own 0..999 filename/sequence and reissue/cancellation contracts.
    pub update_number: Option<u32>,
    pub issue_date: DatasetIssueDate,
}

/// Only the exchange verifier constructs this wrapper, after authenticating
/// both the exact catalogue bytes and the associated resource.
#[derive(Debug, Clone, Serialize)]
pub struct AuthenticatedDatasetDiscovery {
    discovery: DatasetDiscovery,
    resource_path: PathBuf,
    resource_sha384: String,
    resource_size: u64,
    catalogue_path: PathBuf,
    catalogue_sha384: String,
}
impl AuthenticatedDatasetDiscovery {
    pub fn discovery(&self) -> &DatasetDiscovery {
        &self.discovery
    }
    pub fn resource_path(&self) -> &Path {
        &self.resource_path
    }
    pub fn resource_sha384(&self) -> &str {
        &self.resource_sha384
    }
    pub fn resource_size(&self) -> u64 {
        self.resource_size
    }
    pub fn catalogue_path(&self) -> &Path {
        &self.catalogue_path
    }
    pub fn catalogue_sha384(&self) -> &str {
        &self.catalogue_sha384
    }
    pub(super) fn bind(
        discovery: DatasetDiscovery,
        resource: &VerifiedResource,
        catalogue: &VerifiedResource,
    ) -> Self {
        Self {
            discovery,
            resource_path: resource.path.clone(),
            resource_sha384: resource.sha384.clone(),
            resource_size: resource.size,
            catalogue_path: catalogue.path.clone(),
            catalogue_sha384: catalogue.sha384.clone(),
        }
    }
}

/// OFF does not inspect signatures or claim unsignedness. Evaluation explicitly
/// admits an unsigned input. Neither contains unauthenticated catalogue values.
#[derive(Debug, Clone, Serialize)]
pub enum DatasetDiscoveryAuthorization {
    Authenticated(AuthenticatedDatasetDiscovery),
    SignatureVerificationDisabled,
    UnsignedEvaluation,
}

pub(super) fn parse(entry: Node<'_, '_>) -> Result<DatasetDiscovery> {
    fn number(value: &str, label: &str) -> Result<u32> {
        ensure!(
            !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
            "Invalid dataset {label}"
        );
        value
            .parse()
            .with_context(|| format!("Dataset {label} overflow"))
    }
    let purpose = DatasetPurpose::parse(text(unique_child(entry, XC, "purpose")?)?)?;
    let edition_number = number(
        text(unique_child(entry, XC, "editionNumber")?)?,
        "editionNumber",
    )?;
    ensure!(
        edition_number > 0,
        "Dataset catalogue editionNumber must be positive"
    );
    let mut updates = entry.children().filter(|n| is(*n, XC, "updateNumber"));
    let update_number = updates
        .next()
        .map(|n| number(text(n)?, "updateNumber"))
        .transpose()?;
    ensure!(updates.next().is_none(), "Duplicate dataset updateNumber");
    let issue_date = DatasetIssueDate::parse(text(unique_child(entry, XC, "issueDate")?)?)?;
    Ok(DatasetDiscovery {
        purpose,
        edition_number,
        update_number,
        issue_date,
    })
}
