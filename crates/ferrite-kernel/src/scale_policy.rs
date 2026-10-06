//! Product-neutral denominator arithmetic for coverage selection and overscale.
//! S-98 2.0.0 Appendix E uses mathematical scales 1:D; APIs here explicitly use D.
//! Polygon intersection, subtraction, dataset inventory and rendering stay outside.
use anyhow::{ensure, Result};

pub const SCALE_BAND_OPTIMUM_DENOMINATORS: [u32; 15] = [
    10_000_000, 3_500_000, 1_500_000, 700_000, 350_000, 180_000, 90_000, 45_000, 22_000, 12_000,
    8_000, 4_000, 3_000, 2_000, 1_000,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CoverageScaleRange {
    /// None represents no minimum scale (the table's NULL/1:infinity), not zero D.
    pub minimum_denominator: Option<u32>,
    pub optimum_denominator: u32,
    pub maximum_denominator: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct OverscaleState {
    pub factor: f64,
    pub indicator_required: bool,
    pub grossly_overscaled: bool,
    pub pattern_required: bool,
}
impl CoverageScaleRange {
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.optimum_denominator > 0 && self.maximum_denominator > 0,
            "Zero coverage scale denominator"
        );
        ensure!(
            self.maximum_denominator <= self.optimum_denominator,
            "Maximum display scale is smaller than optimum display scale"
        );
        if let Some(min) = self.minimum_denominator {
            ensure!(
                min >= self.optimum_denominator,
                "Minimum display scale is larger than optimum display scale"
            );
        }
        Ok(())
    }
    pub fn within_minimum(self, display_denominator: f64) -> Result<bool> {
        self.validate()?;
        validate_display(display_denominator)?;
        Ok(self
            .minimum_denominator
            .is_none_or(|min| display_denominator <= min as f64))
    }
    /// S-98 E-1.2 strict interval overlap, expressed in denominator order.
    /// This is a selection input; it does not mask an entire dataset by itself.
    pub fn scale_bands(self) -> Result<u16> {
        self.validate()?;
        let upper = self
            .minimum_denominator
            .map(|v| v as u64)
            .unwrap_or(u64::MAX);
        let lower = self.optimum_denominator as u64;
        let mut bands = 0;
        if upper > 10_000_000 {
            bands |= 1;
        }
        for index in 1..15 {
            let band_upper = SCALE_BAND_OPTIMUM_DENOMINATORS[index - 1] as u64;
            let band_lower = SCALE_BAND_OPTIMUM_DENOMINATORS[index] as u64;
            if upper.min(band_upper) > lower.max(band_lower) {
                bands |= 1 << index;
            }
        }
        Ok(bands)
    }
    /// S-98 12.3.1/12.3.3: source data remain visible when overscaled. The
    /// pattern applies to coverage selected to fill gaps (E-1.3 fallback stages).
    pub fn overscale(
        self,
        display_denominator: f64,
        selected_to_fill_gap: bool,
    ) -> Result<OverscaleState> {
        self.validate()?;
        validate_display(display_denominator)?;
        let factor = self.optimum_denominator as f64 / display_denominator;
        let gross = display_denominator < (self.maximum_denominator as f64);
        Ok(OverscaleState {
            factor,
            indicator_required: factor > 1.,
            grossly_overscaled: gross,
            pattern_required: gross && selected_to_fill_gap,
        })
    }
}
fn validate_display(denominator: f64) -> Result<()> {
    ensure!(
        denominator.is_finite() && denominator > 0.,
        "Invalid physical display scale denominator"
    );
    Ok(())
}
/// Band 1 includes the exact 1:10,000,000 endpoint; all subsequent bands
/// include their optimum endpoint and exclude the preceding optimum endpoint.
/// This follows Table E-1's continuous intervals, avoiding a literal ratio/
/// denominator inversion and the pseudocode's uncovered first exact endpoint.
pub fn display_scale_band(display_denominator: f64) -> Result<u8> {
    validate_display(display_denominator)?;
    for (index, optimum) in SCALE_BAND_OPTIMUM_DENOMINATORS.iter().enumerate() {
        if display_denominator >= *optimum as f64 {
            return Ok(index as u8 + 1);
        }
    }
    Ok(15)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scales() -> CoverageScaleRange {
        CoverageScaleRange {
            minimum_denominator: Some(45000),
            optimum_denominator: 22000,
            maximum_denominator: 12000,
        }
    }
    #[test]
    fn minimum_and_overscale_thresholds_do_not_hide_enlarged_data() {
        let s = scales();
        for (d, visible) in [(44999., true), (45000., true), (45001., false)] {
            assert_eq!(s.within_minimum(d).unwrap(), visible);
        }
        for d in [22001., 22000.] {
            assert!(!s.overscale(d, true).unwrap().indicator_required);
        }
        assert!(s.overscale(21999., true).unwrap().indicator_required);
        for d in [12001., 12000.] {
            assert!(!s.overscale(d, true).unwrap().pattern_required);
        }
        assert!(s.overscale(11999., true).unwrap().pattern_required);
        assert!(!s.overscale(11999., false).unwrap().pattern_required);
        assert!(s.within_minimum(1.).unwrap());
        assert_eq!(s.overscale(11000., true).unwrap().factor, 2.);
    }
    #[test]
    fn every_band_boundary_matches_denominator_direction() {
        for (i, d) in SCALE_BAND_OPTIMUM_DENOMINATORS.iter().enumerate() {
            assert_eq!(display_scale_band(*d as f64).unwrap(), i as u8 + 1);
            if i < 14 {
                assert_eq!(display_scale_band(*d as f64 - 0.5).unwrap(), i as u8 + 2);
            }
            assert_eq!(display_scale_band(*d as f64 + 0.5).unwrap(), i as u8 + 1);
        }
        assert_eq!(scales().scale_bands().unwrap(), 1 << 8);
        let global = CoverageScaleRange {
            minimum_denominator: None,
            optimum_denominator: 10000000,
            maximum_denominator: 1000000,
        };
        assert_eq!(global.scale_bands().unwrap(), 1);
        let multi = CoverageScaleRange {
            minimum_denominator: Some(90000),
            optimum_denominator: 12000,
            maximum_denominator: 8000,
        };
        assert_eq!(multi.scale_bands().unwrap(), (1 << 7) | (1 << 8) | (1 << 9));
    }
    #[test]
    fn malformed_scales_never_silently_select_or_drop_data() {
        for d in [0., -1., f64::NAN, f64::INFINITY] {
            assert!(display_scale_band(d).is_err());
            assert!(scales().overscale(d, true).is_err());
        }
        for s in [
            CoverageScaleRange {
                minimum_denominator: Some(1000),
                ..scales()
            },
            CoverageScaleRange {
                maximum_denominator: 45000,
                ..scales()
            },
            CoverageScaleRange {
                optimum_denominator: 0,
                ..scales()
            },
        ] {
            assert!(s.validate().is_err());
        }
    }
}
