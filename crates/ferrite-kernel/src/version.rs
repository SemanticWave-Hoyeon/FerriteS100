//! Numeric S-100 edition/revision/clarification identity, independent of products.
use anyhow::{ensure, Context, Result};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SpecificationVersion {
    pub edition: u32,
    pub revision: u32,
    pub clarification: u32,
}
impl FromStr for SpecificationVersion {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split('.').collect();
        ensure!(
            parts.len() == 2 || parts.len() == 3,
            "Expected edition.revision[.clarification]: {value}"
        );
        let mut numbers = [0u32; 3];
        for (i, part) in parts.iter().enumerate() {
            ensure!(
                !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()),
                "Invalid numeric specification version: {value}"
            );
            numbers[i] = part
                .parse()
                .context("Specification version component overflow")?;
        }
        Ok(Self {
            edition: numbers[0],
            revision: numbers[1],
            clarification: numbers[2],
        })
    }
}
impl SpecificationVersion {
    /// Directional compatibility for a product that guarantees later revisions
    /// can read earlier data within the same Edition. The product adapter decides
    /// whether this policy applies; it is not assumed for every S-100 product.
    pub fn require_same_edition_backward_compatibility(self, dataset: Self) -> Result<()> {
        ensure!(
            self.edition == dataset.edition,
            "Different specification Editions: dataset {dataset:?}, catalogue {self:?}"
        );
        ensure!(
            self >= dataset,
            "Catalogue predates dataset specification: dataset {dataset:?}, catalogue {self:?}"
        );
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_identity_and_directional_compatibility() {
        let v = |s: &str| s.parse::<SpecificationVersion>().unwrap();
        assert_eq!(v("2.0"), v("2.0.0"));
        assert!(v("1.10.0") > v("1.9.8"));
        assert!(v("2.1.0")
            .require_same_edition_backward_compatibility(v("2.0"))
            .is_ok());
        assert!(v("2.0.1")
            .require_same_edition_backward_compatibility(v("2.0.0"))
            .is_ok());
        assert!(v("2.0")
            .require_same_edition_backward_compatibility(v("2.1"))
            .is_err());
        assert!(v("2.0")
            .require_same_edition_backward_compatibility(v("1.1"))
            .is_err());
        for s in [
            "",
            "2",
            "2.0.0.1",
            "2..0",
            "2.-1",
            "2.0-draft",
            " 2.0",
            "4294967296.0",
        ] {
            assert!(s.parse::<SpecificationVersion>().is_err(), "{s}");
        }
    }
}
