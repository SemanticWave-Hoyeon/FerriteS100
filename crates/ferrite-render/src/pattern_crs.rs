//! S-100 Part 9 9-12.5.1.9 / Part 9a AreaCRS pattern anchoring policy.
//! Origin resolution belongs to the output projection, not the product adapter.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PatternCrs {
    Global,
    LocalGeometry,
    #[default]
    GlobalGeometry,
}
impl std::str::FromStr for PatternCrs {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "Global" => Ok(Self::Global),
            "LocalGeometry" => Ok(Self::LocalGeometry),
            "GlobalGeometry" => Ok(Self::GlobalGeometry),
            _ => Err(format!("Unsupported AreaCRS: {value}")),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authored_policy_is_preserved_and_unknown_policy_rejected() {
        for (name, policy) in [
            ("Global", PatternCrs::Global),
            ("LocalGeometry", PatternCrs::LocalGeometry),
            ("GlobalGeometry", PatternCrs::GlobalGeometry),
        ] {
            assert_eq!(name.parse::<PatternCrs>().unwrap(), policy);
        }
        for name in ["", "GLOBAL", "GeographicCRS", "not-a-crs"] {
            assert!(name.parse::<PatternCrs>().is_err());
        }
        assert_eq!(PatternCrs::default(), PatternCrs::GlobalGeometry);
    }
}

/// One ordered stroke of an S-100 hatch line. A two-style HatchFill may expand
/// to several catalogue strokes; preserve their order and independent dashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HatchStroke {
    pub style: crate::LineStyle,
    pub interval_length_mm: f64,
    pub symbols: Box<[HatchLineSymbol]>,
}
pub type HatchLineSymbol = ferrite_kernel::StrokeSymbol;
