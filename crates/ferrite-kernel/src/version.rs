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
    /// Product-opt-in backward Revision compatibility with S98 20.2 equivalent
    /// Clarifications. Clarification numbers do not impose a direction; Edition
    /// must agree and a catalogue Revision may not be older than the dataset.
    /// This is separate from strict lexicographic compatibility so other product
    /// adapters retain their existing contract until explicitly adopting it.
    pub fn require_same_edition_backward_revision_compatibility(self, dataset: Self) -> Result<()> {
        ensure!(
            self.edition == dataset.edition,
            "Different specification Editions: dataset {dataset:?}, catalogue {self:?}"
        );
        ensure!(self.revision >= dataset.revision,
            "Catalogue Revision predates dataset specification: dataset {dataset:?}, catalogue {self:?}");
        Ok(())
    }

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

#[cfg(test)]
mod clarification_equivalence_tests {
    use super::*;
    fn v(s: &str) -> SpecificationVersion {
        s.parse().unwrap()
    }
    #[test]
    fn same_revision_clarifications_are_compatible_in_both_directions() {
        for (a, b) in [
            ("2.0.0", "2.0.9"),
            ("2.0.9", "2.0.0"),
            ("2.0", "2.0.4294967295"),
        ] {
            assert!(v(a)
                .require_same_edition_backward_revision_compatibility(v(b))
                .is_ok());
        }
    }
    #[test]
    fn later_revision_is_allowed_but_earlier_revision_and_other_edition_are_rejected() {
        assert!(v("2.1.0")
            .require_same_edition_backward_revision_compatibility(v("2.0.99"))
            .is_ok());
        assert!(v("2.0.99")
            .require_same_edition_backward_revision_compatibility(v("2.1.0"))
            .is_err());
        assert!(v("3.0")
            .require_same_edition_backward_revision_compatibility(v("2.0"))
            .is_err());
    }
    #[test]
    fn original_strict_public_policy_is_unchanged() {
        assert!(v("2.0.0")
            .require_same_edition_backward_compatibility(v("2.0.1"))
            .is_err());
        assert!(v("2.0.1")
            .require_same_edition_backward_compatibility(v("2.0.0"))
            .is_ok());
    }
}
