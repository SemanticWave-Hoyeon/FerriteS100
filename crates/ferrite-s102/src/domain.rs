//! S102 native-coordinate domain metadata. Raw HDF samples remain unchanged.
#![allow(non_local_definitions)]
use anyhow::{ensure, Result};
use ferrite_kernel::{coverage_domain::SimpleRing, GridGeometry};
use hdf5::{
    types::{FloatSize, TypeDescriptor},
    H5Type,
};
#[derive(Debug, Clone)]
pub enum InstanceDomain {
    FullGrid,
    Rectangle([f64; 4]),
    Polygon(SimpleRing),
}
#[derive(H5Type, Clone, Copy)]
#[repr(C)]
struct LonLat {
    longitude: f64,
    latitude: f64,
}
#[derive(H5Type, Clone, Copy)]
#[repr(C)]
#[allow(non_snake_case)]
struct XY {
    X: f64,
    Y: f64,
}
const NAMES: [&str; 4] = [
    "westBoundLongitude",
    "eastBoundLongitude",
    "southBoundLatitude",
    "northBoundLatitude",
];
// Implementation admission limits, not product-specification limits. Charge all
// rings before decoding any vertices or performing quadratic simplicity checks.
#[derive(Default)]
struct DomainWork {
    vertices: usize,
    pairs: usize,
}
impl DomainWork {
    fn charge(&mut self, count: usize) -> Result<()> {
        ensure!(
            (4..=SimpleRing::MAX_VERTICES + 1).contains(&count),
            "Invalid domain polygon count"
        );
        let edges = count - 1;
        let vertices = self
            .vertices
            .checked_add(count)
            .ok_or_else(|| anyhow::anyhow!("Domain vertex count overflow"))?;
        let pairs = self
            .pairs
            .checked_add(edges * (edges - 1) / 2)
            .ok_or_else(|| anyhow::anyhow!("Domain pair work overflow"))?;
        ensure!(
            vertices <= 16384 && pairs <= 16_777_216,
            "Aggregate domain validation budget exceeded"
        );
        self.vertices = vertices;
        self.pairs = pairs;
        Ok(())
    }
}
pub(crate) fn preflight_work(file: &hdf5::File) -> Result<()> {
    let mut work = DomainWork::default();
    for name in ["BathymetryCoverage", "QualityOfBathymetryCoverage"] {
        if !file.link_exists(name) {
            continue;
        }
        let container = file.group(name)?;
        for instance in container
            .member_names()?
            .into_iter()
            .filter(|n| n.starts_with(&format!("{name}.")))
        {
            let g = container.group(&instance)?;
            if g.link_exists("domainExtent.polygon") {
                let d = g.dataset("domainExtent.polygon")?;
                ensure!(d.ndim() == 1, "Invalid domain polygon dimensions");
                work.charge(d.size())?;
            }
        }
    }
    Ok(())
}
fn adjacent(v: f32, up: bool) -> f32 {
    if v == 0. {
        return if up {
            f32::from_bits(1)
        } else {
            -f32::from_bits(1)
        };
    }
    f32::from_bits(if up == (v > 0.) {
        v.to_bits() + 1
    } else {
        v.to_bits() - 1
    })
}
// A float32 metadata bound represents an encoded-rounded float64 grid boundary.
// Compare its exact nearest-even preimage; no arbitrary tolerance/epsilon snap.
pub(crate) fn matches_encoded(bound: crate::GridBoundary, value: f32) -> Result<bool> {
    let centre = f64::from(value);
    let previous = adjacent(value, false);
    let next = adjacent(value, true);
    let low = if previous.is_finite() {
        (f64::from(previous) + centre) * 0.5
    } else {
        centre - (f64::from(next) - centre) * 0.5
    };
    let high = if next.is_finite() {
        (centre + f64::from(next)) * 0.5
    } else {
        centre + (centre - f64::from(previous)) * 0.5
    };
    let a = crate::compare_boundary(bound, crate::boundary(low, 1., 0.)?)?;
    let b = crate::compare_boundary(bound, crate::boundary(high, 1., 0.)?)?;
    let even = value.to_bits() & 1 == 0;
    Ok(
        (a == std::cmp::Ordering::Greater || even && a == std::cmp::Ordering::Equal)
            && (b == std::cmp::Ordering::Less || even && b == std::cmp::Ordering::Equal),
    )
}
impl InstanceDomain {
    pub fn read(g: &hdf5::Group, geometry: &GridGeometry) -> Result<Self> {
        let attrs = g.attr_names()?;
        let count = NAMES
            .iter()
            .filter(|n| attrs.iter().any(|a| a == **n))
            .count();
        let polygon = g.link_exists("domainExtent.polygon");
        ensure!(count == 0 || count == 4, "Incomplete instance bounding box");
        ensure!(
            (count == 4) != polygon,
            "S102 instance requires either bounding box or domainExtent.polygon, not both/neither"
        );
        if polygon {
            let d = g.dataset("domainExtent.polygon")?;
            ensure!(
                d.ndim() == 1 && (4..=SimpleRing::MAX_VERTICES + 1).contains(&d.size()),
                "Invalid domain polygon shape/count"
            );
            let TypeDescriptor::Compound(c) = d.dtype()?.to_descriptor()? else {
                anyhow::bail!("Domain polygon must be compound coordinates");
            };
            ensure!(
                c.fields.len() == 2
                    && c.fields.iter().all(|f| matches!(
                        f.ty,
                        TypeDescriptor::Float(FloatSize::U4 | FloatSize::U8)
                    )),
                "Domain polygon requires two float coordinates"
            );
            let has = |name: &str| c.fields.iter().any(|f| f.name == name);
            let points = if geometry.horizontal_crs == 4326 && has("longitude") && has("latitude") {
                d.read_raw::<LonLat>()?
                    .into_iter()
                    .map(|p| [p.longitude, p.latitude])
                    .collect()
            } else if geometry.horizontal_crs != 4326 && has("X") && has("Y") {
                d.read_raw::<XY>()?
                    .into_iter()
                    .map(|p| [p.X, p.Y])
                    .collect()
            } else {
                anyhow::bail!("Domain polygon coordinate fields disagree with CRS");
            };
            return Ok(Self::Polygon(SimpleRing::new(points)?));
        }
        let mut encoded = [0f32; 4];
        for (i, name) in NAMES.iter().enumerate() {
            let a = g.attr(name)?;
            ensure!(
                a.dtype()?.to_descriptor()? == TypeDescriptor::Float(FloatSize::U4),
                "Instance bbox must use float32"
            );
            encoded[i] = a.read_scalar::<f32>()?;
        }
        ensure!(
            encoded.iter().all(|v| v.is_finite())
                && encoded[0] <= encoded[1]
                && encoded[2] <= encoded[3],
            "Invalid instance bbox"
        );
        let extent = crate::grid_extent(geometry)?;
        let mut full = true;
        for i in 0..4 {
            let enclosed = crate::compare_boundary(
                extent[i],
                crate::boundary(f64::from(encoded[i]), 1., 0.)?,
            )?;
            full &= matches_encoded(extent[i], encoded[i])?
                || if i % 2 == 0 {
                    enclosed != std::cmp::Ordering::Less
                } else {
                    enclosed != std::cmp::Ordering::Greater
                };
        }
        if full {
            Ok(Self::FullGrid)
        } else {
            ensure!(
                encoded[0] < encoded[1] && encoded[2] < encoded[3],
                "Collapsed clipping bbox"
            );
            Ok(Self::Rectangle(encoded.map(f64::from)))
        }
    }
    pub fn contains(&self, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        match self {
            Self::FullGrid => true,
            Self::Rectangle(b) => x >= b[0] && x <= b[1] && y >= b[2] && y <= b[3],
            Self::Polygon(p) => p.contains([x, y]),
        }
    }
    pub fn requires_mask(&self) -> bool {
        !matches!(self, Self::FullGrid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SERIAL: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(std::path::PathBuf, hdf5::File, hdf5::Group);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn fixture() -> Fixture {
        let p = std::env::temp_dir().join(format!(
            "s102-domain-{}-{}.h5",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let f = hdf5::File::create(&p).unwrap();
        let g = f.create_group("instance").unwrap();
        Fixture(p, f, g)
    }
    fn geometry(crs: u32) -> GridGeometry {
        GridGeometry {
            width: 3,
            height: 2,
            origin_x: 1.,
            origin_y: 2.,
            spacing_x: 1.,
            spacing_y: 1.,
            horizontal_crs: crs,
        }
    }
    fn bbox(g: &hdf5::Group, b: [f32; 4]) {
        for (name, v) in NAMES.into_iter().zip(b) {
            g.new_attr::<f32>()
                .create(name)
                .unwrap()
                .write_scalar(&v)
                .unwrap();
        }
    }
    #[derive(H5Type, Clone, Copy)]
    #[repr(C)]
    struct Small {
        longitude: f32,
        latitude: f32,
    }
    #[derive(H5Type, Clone, Copy)]
    #[repr(C)]
    struct Wrong {
        lon: f64,
        lat: f64,
    }
    #[test]
    fn geographic_and_projected_named_coordinates_decode_at_both_precisions() {
        let f = fixture();
        let points = [
            Small {
                longitude: 0.,
                latitude: 0.,
            },
            Small {
                longitude: 4.,
                latitude: 0.,
            },
            Small {
                longitude: 0.,
                latitude: 4.,
            },
            Small {
                longitude: 0.,
                latitude: 0.,
            },
        ];
        f.2.new_dataset::<Small>()
            .shape(4)
            .create("domainExtent.polygon")
            .unwrap()
            .write_raw(&points)
            .unwrap();
        let d = InstanceDomain::read(&f.2, &geometry(4326)).unwrap();
        assert!(d.contains(1., 1.));
        assert!(d.contains(2., 2.));
        assert!(!d.contains(3., 2.));
        assert!(InstanceDomain::read(&f.2, &geometry(32631)).is_err());
        let f = fixture();
        let points = [
            XY { X: 0., Y: 0. },
            XY { X: 4., Y: 0. },
            XY { X: 0., Y: 4. },
            XY { X: 0., Y: 0. },
        ];
        f.2.new_dataset::<XY>()
            .shape(4)
            .create("domainExtent.polygon")
            .unwrap()
            .write_raw(&points)
            .unwrap();
        assert!(InstanceDomain::read(&f.2, &geometry(32631))
            .unwrap()
            .contains(1., 1.));
        assert!(InstanceDomain::read(&f.2, &geometry(4326)).is_err());
    }
    #[test]
    fn malformed_names_and_two_dimensional_coordinates_are_rejected() {
        let f = fixture();
        let points = [Wrong { lon: 0., lat: 0. }; 4];
        f.2.new_dataset::<Wrong>()
            .shape(4)
            .create("domainExtent.polygon")
            .unwrap()
            .write_raw(&points)
            .unwrap();
        assert!(InstanceDomain::read(&f.2, &geometry(4326)).is_err());
        let f = fixture();
        f.2.new_dataset::<LonLat>()
            .shape([2, 2])
            .create("domainExtent.polygon")
            .unwrap()
            .write_raw(
                &[LonLat {
                    longitude: 0.,
                    latitude: 0.,
                }; 4],
            )
            .unwrap();
        assert!(InstanceDomain::read(&f.2, &geometry(4326)).is_err());
    }
    #[test]
    fn clipping_bbox_is_closed_and_full_grid_is_distinguished() {
        let f = fixture();
        bbox(&f.2, [0.5, 3.5, 1.5, 3.5]);
        assert!(matches!(
            InstanceDomain::read(&f.2, &geometry(4326)).unwrap(),
            InstanceDomain::FullGrid
        ));
        let f = fixture();
        bbox(&f.2, [0.75, 2.75, 1.75, 3.]);
        let d = InstanceDomain::read(&f.2, &geometry(4326)).unwrap();
        assert!(d.requires_mask());
        assert!(d.contains(0.75, 1.75));
        assert!(d.contains(2.75, 3.));
        assert!(!d.contains(f64::from_bits(2.75f64.to_bits() + 1), 2.));
        assert!(!d.contains(f64::NAN, 2.));
    }
    #[test]
    fn nearest_even_metadata_preimage_has_no_epsilon_band() {
        for encoded in [
            0f32,
            -0f32,
            1.,
            f32::from_bits(1f32.to_bits() + 1),
            -1.,
            f32::MIN_POSITIVE,
            f32::MAX,
            -f32::MAX,
        ] {
            assert!(matches_encoded(
                crate::boundary(f64::from(encoded), 1., 0.).unwrap(),
                encoded
            )
            .unwrap());
            let next = adjacent(encoded, true);
            if !next.is_finite() {
                continue;
            }
            let midpoint = (f64::from(encoded) + f64::from(next)) * 0.5;
            assert_eq!(
                matches_encoded(crate::boundary(midpoint, 1., 0.).unwrap(), encoded).unwrap(),
                encoded.to_bits() & 1 == 0
            );
            assert_eq!(
                matches_encoded(crate::boundary(midpoint, 1., 0.).unwrap(), next).unwrap(),
                next.to_bits() & 1 == 0
            );
            let above = if midpoint >= 0. {
                f64::from_bits(midpoint.to_bits() + 1)
            } else {
                f64::from_bits(midpoint.to_bits() - 1)
            };
            assert!(!matches_encoded(crate::boundary(above, 1., 0.).unwrap(), encoded).unwrap());
        }
    }
    #[test]
    fn partial_bbox_wrong_precision_and_nonfinite_bounds_rejected() {
        let f = fixture();
        f.2.new_attr::<f32>()
            .create(NAMES[0])
            .unwrap()
            .write_scalar(&0.)
            .unwrap();
        assert!(InstanceDomain::read(&f.2, &geometry(4326)).is_err());
        let f = fixture();
        for name in NAMES {
            f.2.new_attr::<f64>()
                .create(name)
                .unwrap()
                .write_scalar(&1.)
                .unwrap();
        }
        assert!(InstanceDomain::read(&f.2, &geometry(4326)).is_err());
        let f = fixture();
        bbox(&f.2, [f32::NAN, 3., 1., 3.]);
        assert!(InstanceDomain::read(&f.2, &geometry(4326)).is_err());
    }
    #[test]
    fn aggregate_quadratic_work_is_charged_before_geometry() {
        let mut work = DomainWork::default();
        work.charge(4097).unwrap();
        work.charge(4097).unwrap();
        let previous = (work.vertices, work.pairs);
        assert!(work.charge(4097).is_err());
        assert_eq!((work.vertices, work.pairs), previous);
        assert!(work.charge(usize::MAX).is_err());
        let f = fixture();
        let b = f.1.create_group("BathymetryCoverage").unwrap();
        for i in 1..=3 {
            let g = b
                .create_group(&format!("BathymetryCoverage.{i:02}"))
                .unwrap();
            g.new_dataset::<LonLat>()
                .shape(4097)
                .create("domainExtent.polygon")
                .unwrap();
        }
        assert!(preflight_work(&f.1)
            .unwrap_err()
            .to_string()
            .contains("budget"));
    }
}
