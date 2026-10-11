//! Geographic root metadata is separate from native instance validity.
use crate::{GridBoundary, InstanceDomain};
use anyhow::{ensure, Result};
use ferrite_kernel::GridGeometry;
use hdf5::types::{FloatSize, TypeDescriptor};
const NAMES: [&str; 4] = [
    "westBoundLongitude",
    "eastBoundLongitude",
    "southBoundLatitude",
    "northBoundLatitude",
];
#[derive(Debug, Clone, Copy)]
pub struct RootBounds {
    pub encoded: [f32; 4],
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enclosure {
    Literal,
    /// Original exact boundary can round to the encoded root side (nearest-even
    /// supported numeric profile); not literal decoded-number enclosure.
    Float32Compatible,
    TooSmall,
    /// Native projected units cannot be compared with base geographic degrees.
    UnverifiedProjected,
    /// Wrapped/unwrapped sheet relationship requires a periodic CRS contract.
    UnverifiedLongitudeSheet,
    /// Exact native footprint exceeds the geographic latitude axis.
    OutsideGeographicLatitudeRange,
    /// Periodic root metadata interpretation; not verified S102 authoring conformance.
    PeriodicLiteral,
    /// Periodic interpretation with the explicitly supported nearest-even profile.
    PeriodicFloat32Compatible,
    /// Periodic interpretation fails enclosure; product encoding remains qualified.
    PeriodicTooSmall,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootEnclosure {
    pub full_grid: Enclosure,
    pub declared_domain: Enclosure,
}
impl RootEnclosure {
    pub fn encoding_compatible(self) -> bool {
        [self.full_grid, self.declared_domain]
            .into_iter()
            .all(|v| matches!(v, Enclosure::Literal | Enclosure::Float32Compatible))
    }
}
fn contains(bounds: [f32; 4], extent: [GridBoundary; 4]) -> Result<Enclosure> {
    let mut compatible = false;
    for i in 0..4 {
        let c = crate::compare_boundary(extent[i], crate::boundary(f64::from(bounds[i]), 1., 0.)?)?;
        let literal = if i % 2 == 0 {
            c != std::cmp::Ordering::Less
        } else {
            c != std::cmp::Ordering::Greater
        };
        if !literal {
            if !crate::domain::matches_encoded(extent[i], bounds[i])? {
                return Ok(Enclosure::TooSmall);
            }
            compatible = true;
        }
    }
    Ok(if compatible {
        Enclosure::Float32Compatible
    } else {
        Enclosure::Literal
    })
}
// Use canonical source bounds without translating their exact expansions.
// GGXF documents this ISO-style root interpretation; S102-specific authoring
// permission is unverified. Distinct results deliberately fail the strict gate.
fn contains_periodic(bounds: [f32; 4], extent: [GridBoundary; 4]) -> Result<Enclosure> {
    let arc = ferrite_kernel::longitude_extent::LongitudeArc::new(
        f64::from(bounds[0]),
        f64::from(bounds[1]),
    )?;
    let (segments, count) = arc.segments();
    let mut compatible = false;
    for segment in &segments[..count] {
        let b = [segment[0] as f32, segment[1] as f32, bounds[2], bounds[3]];
        match contains(b, extent)? {
            Enclosure::Literal => return Ok(Enclosure::PeriodicLiteral),
            Enclosure::Float32Compatible => compatible = true,
            _ => {}
        }
    }
    Ok(if compatible {
        Enclosure::PeriodicFloat32Compatible
    } else {
        Enclosure::PeriodicTooSmall
    })
}
fn assess_geographic(bounds: [f32; 4], extent: [GridBoundary; 4]) -> Result<Enclosure> {
    use std::cmp::Ordering::{Greater, Less};
    // Encoding roundoff must not license native coordinates outside the latitude axis.
    if crate::compare_boundary(extent[2], crate::boundary(-90., 1., 0.)?)? == Less
        || crate::compare_boundary(extent[3], crate::boundary(90., 1., 0.)?)? == Greater
    {
        return Ok(Enclosure::OutsideGeographicLatitudeRange);
    }
    if crate::compare_boundary(extent[0], crate::boundary(-180., 1., 0.)?)? == Less
        || crate::compare_boundary(extent[1], crate::boundary(180., 1., 0.)?)? == Greater
    {
        return Ok(Enclosure::UnverifiedLongitudeSheet);
    }
    let periodic = bounds[0] > bounds[1] || (bounds[0] == bounds[1] && bounds[0].abs() == 180.);
    if periodic {
        contains_periodic(bounds, extent)
    } else {
        contains(bounds, extent)
    }
}
impl RootBounds {
    pub fn read(file: &hdf5::File) -> Result<Self> {
        let mut encoded = [0.; 4];
        for (i, n) in NAMES.iter().enumerate() {
            let a = file.attr(n)?;
            ensure!(
                a.dtype()?.to_descriptor()? == TypeDescriptor::Float(FloatSize::U4),
                "S102 root bbox requires float32 {n}"
            );
            encoded[i] = crate::singleton::read::<f32>(&a)?;
        }
        ensure!(
            encoded.iter().all(|v| v.is_finite()),
            "Nonfinite geographic root bbox"
        );
        ensure!(
            encoded[0] >= -180.
                && encoded[0] <= 180.
                && encoded[1] >= -180.
                && encoded[1] <= 180.
                && encoded[2] >= -90.
                && encoded[3] <= 90.
                && encoded[2] <= encoded[3],
            "Invalid geographic root bbox"
        );
        // west>east remains explicitly encoded, never sorted or silently normalized.
        Ok(Self { encoded })
    }
    pub(crate) fn assess(
        self,
        g: &GridGeometry,
        domain: &InstanceDomain,
        instance: &hdf5::Group,
    ) -> Result<RootEnclosure> {
        if g.horizontal_crs != 4326 {
            return Ok(RootEnclosure {
                full_grid: Enclosure::UnverifiedProjected,
                declared_domain: Enclosure::UnverifiedProjected,
            });
        }
        let grid = crate::grid_extent(g)?;
        let raw = match domain {
            InstanceDomain::Polygon(p) => p.bounds(),
            InstanceDomain::Rectangle(b) => *b,
            InstanceDomain::FullGrid => {
                let mut b = [0.; 4];
                for (i, n) in NAMES.iter().enumerate() {
                    b[i] = f64::from(crate::singleton::read::<f32>(&instance.attr(n)?)?);
                }
                b
            }
        };
        let declared = [
            crate::boundary(raw[0], 1., 0.)?,
            crate::boundary(raw[1], 1., 0.)?,
            crate::boundary(raw[2], 1., 0.)?,
            crate::boundary(raw[3], 1., 0.)?,
        ];
        Ok(RootEnclosure {
            full_grid: assess_geographic(self.encoded, grid)?,
            declared_domain: assess_geographic(self.encoded, declared)?,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn exact(b: [f64; 4]) -> [GridBoundary; 4] {
        b.map(|v| crate::boundary(v, 1., 0.).unwrap())
    }
    #[test]
    fn float32_enclosure_is_directional_and_has_no_epsilon_band() {
        let inside = 1. + 2f64.powi(-25);
        let root = [1f32, 2., 0., 1.];
        assert_eq!(
            contains(root, exact([inside, 2., 0., 1.])).unwrap(),
            Enclosure::Literal
        );
        let lower = 1. - 2f64.powi(-26);
        assert_eq!(
            contains(root, exact([lower, 2., 0., 1.])).unwrap(),
            Enclosure::Float32Compatible
        );
        let next = f32::from_bits(1f32.to_bits() + 1);
        assert_eq!(
            contains([next, 2., 0., 1.], exact([inside, 2., 0., 1.])).unwrap(),
            Enclosure::TooSmall
        );
        let midpoint = (1. + f64::from(f32::from_bits(1f32.to_bits() - 1))) * 0.5;
        assert_eq!(
            contains(root, exact([midpoint, 2., 0., 1.])).unwrap(),
            Enclosure::Float32Compatible
        );
        assert_eq!(
            contains(
                root,
                exact([f64::from_bits(midpoint.to_bits() - 1), 2., 0., 1.])
            )
            .unwrap(),
            Enclosure::TooSmall
        );
    }
    #[test]
    fn footprint_checks_all_fill_cells_instead_of_only_domain() {
        let root = [1., 2., 1., 2.];
        assert_eq!(
            contains(root, exact([0.5, 2.5, 0.5, 2.5])).unwrap(),
            Enclosure::TooSmall
        );
        assert_eq!(
            contains(root, exact([1.25, 1.75, 1.25, 1.75])).unwrap(),
            Enclosure::Literal
        );
    }
    #[test]
    fn decoded_declared_geometry_is_checked_separately() {
        let root = [0., 2., 0., 2.];
        assert_eq!(
            contains(root, exact([-0.5, 2.5, 0., 2.])).unwrap(),
            Enclosure::TooSmall
        );
        assert_eq!(
            contains(root, exact([0.5, 1.5, 0.5, 1.5])).unwrap(),
            Enclosure::Literal
        );
    }
    #[test]
    fn root_attributes_are_mandatory_float32_geographic_coordinates() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        for case in 0..5 {
            let path = std::env::temp_dir().join(format!(
                "s102-root-{}-{}.h5",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            let f = hdf5::File::create(&path).unwrap();
            for (i, n) in NAMES.iter().enumerate() {
                if case == 1 && i == 0 {
                    continue;
                }
                let v = if case == 2 && i == 0 {
                    f32::NAN
                } else if case == 3 && i == 3 {
                    91.
                } else {
                    [0., 1., 0., 1.][i]
                };
                if case == 4 {
                    f.new_attr::<f64>()
                        .create(*n)
                        .unwrap()
                        .write_scalar(&f64::from(v))
                        .unwrap();
                } else {
                    f.new_attr::<f32>()
                        .create(*n)
                        .unwrap()
                        .write_scalar(&v)
                        .unwrap();
                }
            }
            assert_eq!(RootBounds::read(&f).is_ok(), case == 0);
            drop(f);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn periodic_root_checks_whole_footprint_and_latitudes_without_shortening() {
        let root = [170., -170., -2., 2.];
        for b in [
            [175., 180., -1., 1.],
            [-180., -175., -1., 1.],
            [170., 170., -2., 2.],
        ] {
            assert_eq!(
                contains_periodic(root, exact(b)).unwrap(),
                Enclosure::PeriodicLiteral
            );
        }
        for b in [
            [-175., 175., -1., 1.],
            [169., 175., -1., 1.],
            [-175., -169., -1., 1.],
            [175., 179., -3., 1.],
        ] {
            assert_eq!(
                contains_periodic(root, exact(b)).unwrap(),
                Enclosure::PeriodicTooSmall
            );
        }
        assert_eq!(
            contains_periodic([180., -180., -2., 2.], exact([180., 180., 0., 1.])).unwrap(),
            Enclosure::PeriodicLiteral
        );
        assert_eq!(
            contains_periodic([180., -180., -2., 2.], exact([-180., 180., 0., 1.])).unwrap(),
            Enclosure::PeriodicTooSmall
        );
    }
    #[test]
    fn periodic_float32_preimage_keeps_exact_boundary_residuals() {
        let root = [170., -170., -2., 2.];
        let next = f32::from_bits(170f32.to_bits() - 1);
        let midpoint = (170. + f64::from(next)) * 0.5;
        assert_eq!(
            contains_periodic(root, exact([midpoint, 175., 0., 1.])).unwrap(),
            Enclosure::PeriodicFloat32Compatible
        );
        assert_eq!(
            contains_periodic(
                root,
                exact([f64::from_bits(midpoint.to_bits() - 1), 175., 0., 1.])
            )
            .unwrap(),
            Enclosure::PeriodicTooSmall
        );
        // Exact sum is below the rounding midpoint although rounded coordinate equals it.
        let mut b = exact([midpoint, 175., 0., 1.]);
        b[0] = crate::boundary(midpoint, -2f64.powi(-80), 1.).unwrap();
        assert_eq!(b[0].rounded, midpoint);
        assert_eq!(
            contains_periodic(root, b).unwrap(),
            Enclosure::PeriodicTooSmall
        );
        for v in [
            Enclosure::PeriodicLiteral,
            Enclosure::PeriodicFloat32Compatible,
            Enclosure::PeriodicTooSmall,
        ] {
            assert!(!RootEnclosure {
                full_grid: v,
                declared_domain: v
            }
            .encoding_compatible());
        }
    }

    #[test]
    fn latitude_rounding_never_licenses_out_of_axis_footprints() {
        let mut b = exact([175., 179., 89., 90.]);
        b[3] = crate::boundary(90., 2f64.powi(-80), 1.).unwrap();
        assert_eq!(b[3].rounded, 90.);
        assert_eq!(
            contains([170., 180., 88., 90.], b).unwrap(),
            Enclosure::Float32Compatible
        );
        for root in [[170., 180., 88., 90.], [170., -170., 88., 90.]] {
            assert_eq!(
                assess_geographic(root, b).unwrap(),
                Enclosure::OutsideGeographicLatitudeRange
            );
        }
        let mut b = exact([175., 179., -90., -89.]);
        b[2] = crate::boundary(-90., -2f64.powi(-80), 1.).unwrap();
        assert_eq!(
            assess_geographic([170., -170., -90., -88.], b).unwrap(),
            Enclosure::OutsideGeographicLatitudeRange
        );
        assert_eq!(
            assess_geographic([170., -170., -90., 90.], exact([175., 179., -90., 90.])).unwrap(),
            Enclosure::PeriodicLiteral
        );
    }
}
