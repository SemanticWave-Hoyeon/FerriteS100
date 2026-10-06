//! Product-neutral S-100 Part 9 augmented-point conversion.
//! Local axes parallel portrayal axes; authored millimetres are never degrees.
//! The host supplies the portrayal origin in device pixels and a geographic
//! projector. Map zoom, rotation and longitude copies belong to that projector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AugmentedPointPosition {
    Geographic([f64; 2]),
    /// Product adapters must obtain this reference from a point feature, not a
    /// centroid or the first vertex of a curve/surface (9-11.1.13).
    Local {
        reference_point: [f64; 2],
        millimetres: [f64; 2],
    },
    Portrayal {
        millimetres: [f64; 2],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionError(pub &'static str);
impl std::fmt::Display for PositionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for PositionError {}

/// Coordinates returned by the projector and resolver share device pixels.
/// Device X increases rightwards and device Y downwards. The host sets origin
/// explicitly: no origin convention is inferred from geographic bounds.
#[derive(Debug, Clone, Copy)]
pub struct PortrayalDevice {
    origin: [f64; 2],
    pixels_per_mm: [f64; 2],
}
fn finite(p: [f64; 2]) -> Result<[f64; 2], PositionError> {
    if p.iter().all(|v| v.is_finite()) {
        Ok(p)
    } else {
        Err(PositionError("Non-finite portrayal position"))
    }
}
impl PortrayalDevice {
    pub fn new(origin: [f64; 2], pixels_per_mm: [f64; 2]) -> Result<Self, PositionError> {
        finite(origin)?;
        finite(pixels_per_mm)?;
        if pixels_per_mm.iter().any(|v| *v <= 0.) {
            return Err(PositionError("Physical display density must be positive"));
        }
        Ok(Self {
            origin,
            pixels_per_mm,
        })
    }
    fn displaced(&self, origin: [f64; 2], mm: [f64; 2]) -> Result<[f64; 2], PositionError> {
        finite(origin)?;
        finite(mm)?;
        // S-100 9-12.2.2.7: positive portrayal Y points upwards.
        finite([
            origin[0] + mm[0] * self.pixels_per_mm[0],
            origin[1] - mm[1] * self.pixels_per_mm[1],
        ])
    }
    /// None means an invisible source point, e.g. behind the globe horizon.
    /// Glyph LocalOffset is separate and must not alter this coverage origin.
    pub fn resolve(
        &self,
        position: AugmentedPointPosition,
        project: impl FnOnce([f64; 2]) -> Result<Option<[f64; 2]>, PositionError>,
    ) -> Result<Option<[f64; 2]>, PositionError> {
        match position {
            AugmentedPointPosition::Portrayal { millimetres } => {
                self.displaced(self.origin, millimetres).map(Some)
            }
            AugmentedPointPosition::Geographic(coordinates) => {
                finite(coordinates)?;
                project(coordinates)?.map(finite).transpose()
            }
            AugmentedPointPosition::Local {
                reference_point,
                millimetres,
            } => {
                finite(reference_point)?;
                finite(millimetres)?;
                project(reference_point)?
                    .map(|p| self.displaced(p, millimetres))
                    .transpose()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_mm_remain_physical_through_zoom_rotation_and_pan() {
        // Different OS pixel densities and 200x zoom must not multiply mm by
        // the map scale. Exercise map rotation as part of anchor projection.
        for density in [1., 1.25, 1.5, 2., 3.] {
            let ppm = 96. * density / 25.4;
            let device = PortrayalDevice::new([0., 1080.], [ppm, ppm]).unwrap();
            for zoom in [0.01, 1., 4., 200.] {
                for bearing in [0_f64, 0.4, 1.57, 3.14] {
                    let anchor = [100. + zoom * bearing.cos(), 75. + zoom * bearing.sin()];
                    let actual = device
                        .resolve(
                            AugmentedPointPosition::Local {
                                reference_point: [127., 35.],
                                millimetres: [3.2, -1.5],
                            },
                            |p| {
                                assert_eq!(p, [127., 35.]);
                                Ok(Some(anchor))
                            },
                        )
                        .unwrap()
                        .unwrap();
                    assert!((actual[0] - anchor[0] - 3.2 * ppm).abs() < 1e-10);
                    assert!((actual[1] - anchor[1] - 1.5 * ppm).abs() < 1e-10);
                }
            }
        }
    }
    #[test]
    fn device_position_does_not_follow_map_projection_or_longitude_copies() {
        let d = PortrayalDevice::new([20., 800.], [4., 5.]).unwrap();
        assert_eq!(
            d.resolve(
                AugmentedPointPosition::Portrayal {
                    millimetres: [10., 20.]
                },
                |_| panic!("Output-device positions must not be projected or wrapped")
            )
            .unwrap(),
            Some([60., 700.])
        );
    }
    #[test]
    fn hidden_local_origin_stays_hidden_and_projector_errors_are_preserved() {
        let d = PortrayalDevice::new([0., 800.], [4., 4.]).unwrap();
        let p = AugmentedPointPosition::Local {
            reference_point: [0., 0.],
            millimetres: [1e6, 0.],
        };
        assert_eq!(d.resolve(p, |_| Ok(None)).unwrap(), None);
        assert_eq!(
            d.resolve(p, |_| Err(PositionError("Horizon failure")))
                .unwrap_err(),
            PositionError("Horizon failure")
        );
    }
    #[test]
    fn geographic_coordinates_are_forwarded_once_without_axis_swap() {
        let d = PortrayalDevice::new([0., 800.], [4., 4.]).unwrap();
        let mut calls = 0;
        assert_eq!(
            d.resolve(AugmentedPointPosition::Geographic([181., 50.]), |p| {
                calls += 1;
                assert_eq!(p, [181., 50.]);
                Ok(Some([30., 40.]))
            })
            .unwrap(),
            Some([30., 40.])
        );
        assert_eq!(calls, 1);
    }
    #[test]
    fn invalid_authored_values_and_overflow_fail_before_rendering() {
        for density in [0., -1., f64::NAN, f64::INFINITY] {
            assert!(PortrayalDevice::new([0., 0.], [density, 4.]).is_err());
        }
        let d = PortrayalDevice::new([0., 800.], [4., 4.]).unwrap();
        for x in [f64::NAN, f64::INFINITY, f64::MAX] {
            assert!(d
                .resolve(
                    AugmentedPointPosition::Portrayal {
                        millimetres: [x, 0.]
                    },
                    |_| panic!()
                )
                .is_err());
        }
        assert!(d
            .resolve(
                AugmentedPointPosition::Local {
                    reference_point: [f64::NAN, 0.],
                    millimetres: [0., 0.]
                },
                |_| panic!("Invalid anchor must be rejected before projection")
            )
            .is_err());
        assert!(d
            .resolve(AugmentedPointPosition::Geographic([0., 0.]), |_| Ok(Some(
                [f64::INFINITY, 0.]
            )))
            .is_err());
    }
}
