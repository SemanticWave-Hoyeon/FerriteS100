//! S-101 coverage inventory projected by the same flat view as chart geometry.
//! Keep extraction outside navigation; device projection and selection are per view.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::coverage_frame::CoverageFrame;
use ferrite_kernel::coverage_selection::{CoverageFootprint, Region};
use ferrite_render::{
    InstructionCoverageClass, PreparedCoverage, PreparedCoveragePass, RenderContext, RenderError,
    Scaler, WorldPoint,
};
use ferrite_s100_core::S101Cell;
use std::{collections::BTreeSet, sync::Arc};

#[derive(Debug)]
pub struct GeographicCoverageInventory {
    datasets: Vec<Option<Vec<crate::coverage_geometry::DataCoverage>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::{GeoBounds, PointInstruction, PointOriginCrs, PortrayalOrigin, Viewport};
    fn inventory() -> GeographicCoverageInventory {
        GeographicCoverageInventory {
            datasets: vec![Some(vec![crate::coverage_geometry::DataCoverage {
                feature_key: 1,
                drawing_index: None,
                scales: ferrite_kernel::scale_policy::CoverageScaleRange {
                    minimum_denominator: Some(180000),
                    optimum_denominator: 90000,
                    maximum_denominator: 45000,
                },
                surfaces: vec![crate::coverage_geometry::GeographicSurface {
                    exterior: vec![[0., 0.], [1., 0.], [1., 1.], [0., 1.], [0., 0.]],
                    holes: vec![],
                }],
            }])],
        }
    }
    fn context(cell: Option<usize>, origin: PortrayalOrigin) -> RenderContext {
        let mut context = RenderContext::new(Viewport::new(64., 40.));
        context.set_bounds(GeoBounds::new(0., 0., 1., 1.));
        let mut point = PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0.5, 0.5));
        if let Some(cell) = cell {
            point = point.with_cell_index(cell);
        }
        let mut point = ferrite_render::DrawingInstruction::Point(point);
        point.set_portrayal_origin(origin);
        context.add_instruction(point);
        context.get_sorted_instructions();
        context
    }
    #[test]
    fn missing_or_unknown_dataset_cannot_bypass_coverage() {
        let source = inventory();
        for cell in [None, Some(99)] {
            let context = context(cell, PortrayalOrigin::NonPoint);
            assert!(source
                .prepare_flat(&context, &BTreeSet::new(), [64, 40], false, 10000)
                .is_err());
        }
        let context = context(None, PortrayalOrigin::Unspecified);
        let prepared = source
            .prepare_flat(&context, &BTreeSet::from([0]), [64, 40], false, 10000)
            .unwrap()
            .unwrap();
        assert_eq!(
            prepared.pass(0).unwrap().decision(0).unwrap(),
            ferrite_kernel::coverage_frame::FrameCoverageDecision::Unclipped
        );
        assert!(source
            .prepare_flat(&context, &BTreeSet::from([1]), [64, 40], false, 10000)
            .is_err());
    }
    #[test]
    fn portrayal_device_origin_is_only_drawn_in_the_central_pass() {
        let source = inventory();
        let context = context(
            Some(0),
            PortrayalOrigin::augmented_point(PointOriginCrs::Portrayal, [5., 5.]).unwrap(),
        );
        let prepared = source
            .prepare_flat(&context, &BTreeSet::new(), [64, 40], true, 10000)
            .unwrap()
            .unwrap();
        use ferrite_kernel::coverage_frame::FrameCoverageDecision;
        assert_eq!(
            prepared.pass(0).unwrap().decision(0).unwrap(),
            FrameCoverageDecision::Unclipped
        );
        assert_eq!(
            prepared.pass(1).unwrap().decision(0).unwrap(),
            FrameCoverageDecision::Hidden
        );
        assert_eq!(
            prepared.pass(2).unwrap().decision(0).unwrap(),
            FrameCoverageDecision::Hidden
        );
    }
    #[test]
    fn physical_crs_must_not_be_interpreted_as_geographic_degrees() {
        let source = inventory();
        for crs in [PointOriginCrs::Local] {
            let context = context(
                Some(0),
                PortrayalOrigin::augmented_point(crs, [0.5, 0.5]).unwrap(),
            );
            assert!(source
                .prepare_flat(&context, &BTreeSet::new(), [64, 40], false, 10000)
                .is_err());
        }
    }
}
impl GeographicCoverageInventory {
    /// Legacy Edition 1 has no current regional scale model and remains explicit.
    pub fn from_cells(cells: &[S101Cell]) -> Result<Self> {
        let mut datasets = Vec::with_capacity(cells.len());
        for cell in cells {
            let version: ferrite_kernel::SpecificationVersion =
                cell.dsid.product_edition.parse()?;
            datasets.push(match version.edition {
                1 => None,
                2 => Some(crate::coverage_geometry::extract_current_coverages(cell)?),
                _ => anyhow::bail!("Unsupported S-101 coverage edition {}", version.edition),
            });
        }
        Ok(Self { datasets })
    }

    pub fn current_dataset_count(&self) -> usize {
        self.datasets.iter().filter(|d| d.is_some()).count()
    }

    /// All longitude copies participate in one selection in device coordinates.
    /// A dataset retains one identity even when its footprint crosses the seam.
    pub fn project(&self, scaler: &Scaler, wrapping: bool) -> Result<Vec<CoverageFootprint>> {
        let mut inventory = Vec::new();
        for (dataset_id, coverages) in self.datasets.iter().enumerate() {
            let Some(coverages) = coverages else { continue };
            for coverage in coverages {
                let mut region: Option<Region> = None;
                for surface in &coverage.surfaces {
                    for &shift in &[0., -360., 360.][..if wrapping { 3 } else { 1 }] {
                        let project = |ring: &[[f64; 2]]| -> Result<Vec<[f64; 2]>> {
                            ring.iter()
                                .map(|p| {
                                    let p =
                                        scaler.world_to_screen(WorldPoint::new(p[0] + shift, p[1]));
                                    let p = [f64::from(p.x), f64::from(p.y)];
                                    ensure!(
                                        p.iter().all(|v| v.is_finite()),
                                        "Non-finite coverage projection"
                                    );
                                    Ok(p)
                                })
                                .collect()
                        };
                        let exterior = project(&surface.exterior)?;
                        let holes = surface
                            .holes
                            .iter()
                            .map(|ring| project(ring))
                            .collect::<Result<Vec<_>>>()?;
                        let part = Region::from_rings(&exterior, &holes)?;
                        region = Some(match region {
                            Some(r) => r.union(&part),
                            None => part,
                        });
                    }
                }
                inventory.push(CoverageFootprint {
                    dataset_id,
                    coverage_id: coverage.feature_key,
                    scales: coverage.scales,
                    region: region.context("Empty S-101 DataCoverage surface")?,
                });
            }
        }
        Ok(inventory)
    }

    /// The caller sorts first and explicitly identifies overlay ordinals in that
    /// immutable instruction order. Missing chart identity never implies exemption.
    pub fn prepare_flat(
        &self,
        context: &RenderContext,
        exempt_ordinals: &BTreeSet<usize>,
        extent: [u32; 2],
        wrapping: bool,
        pixel_budget: usize,
    ) -> Result<Option<PreparedCoverage>> {
        ensure!(
            exempt_ordinals
                .iter()
                .all(|i| *i < context.instruction_count()),
            "Overlay ordinal outside context"
        );
        if self.current_dataset_count() == 0 {
            return Ok(None);
        }
        let scaler = &context.scaler;
        let inventory = self.project(scaler, wrapping)?;
        let v = scaler.viewport;
        let [x, y, right, bottom] = [
            v.x as f64,
            v.y as f64,
            (v.x + v.width) as f64,
            (v.y + v.height) as f64,
        ];
        let viewport = Region::from_rings(
            &[[x, y], [right, y], [right, bottom], [x, bottom], [x, y]],
            &[],
        )?;
        let plan =
            crate::coverage_loading::display_plan(&inventory, scaler.display_scale, &viewport)?;
        let frame = Arc::new(CoverageFrame::new(
            &plan.eligible_inventory,
            &plan.selection,
            &viewport,
            extent,
            pixel_budget,
        )?);
        // PreparedCoverage and its caller share the already-sorted raw order.
        let mut passes = Vec::new();
        for &shift in &[0., -360., 360.][..if wrapping { 3 } else { 1 }] {
            let mut ordinal = 0;
            let pass = PreparedCoveragePass::prepare(
                context.raw_instructions(),
                frame.clone(),
                |command| {
                    let index = ordinal;
                    ordinal += 1;
                    if exempt_ordinals.contains(&index) {
                        return Ok(InstructionCoverageClass::Exempt);
                    }
                    let cell = command.cell_index().ok_or_else(|| {
                        RenderError::Render("Chart command missing dataset identity".into())
                    })? as usize;
                    let dataset = self.datasets.get(cell).ok_or_else(|| {
                        RenderError::Render("Chart command refers to unknown dataset".into())
                    })?;
                    Ok(if dataset.is_some() {
                        InstructionCoverageClass::Dataset(cell)
                    } else {
                        InstructionCoverageClass::Exempt
                    })
                },
                |_, origin| {
                    ferrite_render::PortrayalOrigin::project_flat_source(origin, scaler, shift)
                        .map(|p| p.map(|p| [p.x as f64, p.y as f64]))
                },
            )?;
            passes.push(pass);
        }
        Ok(Some(PreparedCoverage::new(
            context.geometry_revision(),
            context.coverage_view_revision(),
            context.instruction_count(),
            passes,
        )?))
    }
}

#[cfg(test)]
mod anchored_local_coverage_tests {
    use super::*;
    use ferrite_render::{GeoBounds, PointInstruction, PortrayalOrigin, Viewport};
    #[test]
    fn authored_local_point_can_leave_the_finer_coverage_without_moving_feature() {
        // A finer dataset occupies the left half of the viewport. The reference
        // feature lies in its interior, but the augmented point is displaced
        // physically into uncovered water on the right. Hide the former only.
        let source = GeographicCoverageInventory {
            datasets: vec![
                Some(vec![crate::coverage_geometry::DataCoverage {
                    feature_key: 1,
                    drawing_index: None,
                    scales: ferrite_kernel::scale_policy::CoverageScaleRange {
                        minimum_denominator: Some(180000),
                        optimum_denominator: 90000,
                        maximum_denominator: 45000,
                    },
                    surfaces: vec![crate::coverage_geometry::GeographicSurface {
                        exterior: vec![[0., 0.], [1., 0.], [1., 1.], [0., 1.], [0., 0.]],
                        holes: vec![],
                    }],
                }]),
                Some(vec![crate::coverage_geometry::DataCoverage {
                    feature_key: 2,
                    drawing_index: None,
                    scales: ferrite_kernel::scale_policy::CoverageScaleRange {
                        minimum_denominator: Some(45000),
                        optimum_denominator: 12000,
                        maximum_denominator: 4000,
                    },
                    surfaces: vec![crate::coverage_geometry::GeographicSurface {
                        exterior: vec![[0., 0.], [0.5, 0.], [0.5, 1.], [0., 1.], [0., 0.]],
                        holes: vec![],
                    }],
                }]),
            ],
        };
        let mut c = RenderContext::new(Viewport::new(100., 100.));
        c.set_bounds(GeoBounds::new(0., 0., 1., 1.));
        c.scaler.display_scale = 12000.;
        let anchor = WorldPoint::new(0.25, 0.5);
        for mm in [[0., 0.], [10., 0.]] {
            let mut p = PointInstruction::new("ACHBRT07".into(), anchor).with_cell_index(0);
            p.portrayal_origin = PortrayalOrigin::augmented_local_point(anchor, mm).unwrap();
            c.add_instruction(ferrite_render::DrawingInstruction::Point(p));
        }
        c.get_sorted_instructions();
        let prepared = source
            .prepare_flat(&c, &BTreeSet::new(), [100, 100], false, 100000)
            .unwrap()
            .unwrap();
        let decisions: Vec<_> = (0..2)
            .map(|i| prepared.pass(0).unwrap().decision(i).unwrap())
            .collect();
        assert!(decisions.contains(&ferrite_kernel::coverage_frame::FrameCoverageDecision::Hidden));
        assert!(
            decisions.contains(&ferrite_kernel::coverage_frame::FrameCoverageDecision::Unclipped)
        );
        // The old decision cannot be affine-scaled: the 10mm displacement is
        // screen-fixed. Both augmented points enter finer coverage after zoom.
        for zoom in [4., 200.] {
            let width = 1. / zoom;
            c.set_bounds(GeoBounds::new(
                0.25 - width / 2.,
                0.5 - width / 2.,
                0.25 + width / 2.,
                0.5 + width / 2.,
            ));
            c.scaler.display_scale = 12000.;
            let next = source
                .prepare_flat(&c, &BTreeSet::new(), [100, 100], false, 100000)
                .unwrap()
                .unwrap();
            for i in 0..2 {
                assert_eq!(
                    next.pass(0).unwrap().decision(i).unwrap(),
                    ferrite_kernel::coverage_frame::FrameCoverageDecision::Hidden
                );
            }
            assert!(c
                .raw_instructions()
                .iter()
                .all(|i| i.portrayal_origin().requires_view_reprojection()));
        }
    }
}

/// An actual device projection supplied by the view backend. The same physical
/// pixel coordinates must be used for coverage surfaces, point origins and the
/// drawing/picking passes. A globe caller invalidates the context coverage view
/// on every camera change before constructing this immutable binding.
pub struct ProjectedCoverageView<'a> {
    pub viewport: &'a Region,
    pub extent: [u32; 2],
    pub display_scale: f64,
    pub pixel_budget: usize,
}
impl GeographicCoverageInventory {
    /// Project complete component surfaces, including holes, through a caller's
    /// camera. Dataset identity and producer scales cannot be replaced by that
    /// callback. An empty projected region is valid for geometry behind the
    /// horizon; a failed projection is propagated, never retried as flat.
    pub fn project_with(
        &self,
        mut project: impl FnMut(
            usize,
            i64,
            &[crate::coverage_geometry::GeographicSurface],
        ) -> Result<Region>,
    ) -> Result<Vec<CoverageFootprint>> {
        let mut inventory = Vec::new();
        for (dataset_id, coverages) in self.datasets.iter().enumerate() {
            let Some(coverages) = coverages else { continue };
            for coverage in coverages {
                ensure!(
                    !coverage.surfaces.is_empty(),
                    "Empty S-101 DataCoverage surface"
                );
                coverage.scales.validate()?;
                inventory.push(CoverageFootprint {
                    dataset_id,
                    coverage_id: coverage.feature_key,
                    scales: coverage.scales,
                    region: project(dataset_id, coverage.feature_key, &coverage.surfaces)?,
                });
            }
        }
        Ok(inventory)
    }

    /// Bind a single camera projection in the already sorted instruction order.
    /// The backend must project the typed source origin, including authored local
    /// offsets and device CRS, rather than substituting a feature's position.
    pub fn prepare_projected(
        &self,
        context: &RenderContext,
        exempt_ordinals: &BTreeSet<usize>,
        view: ProjectedCoverageView<'_>,
        project_surfaces: impl FnMut(
            usize,
            i64,
            &[crate::coverage_geometry::GeographicSurface],
        ) -> Result<Region>,
        mut project_origin: impl FnMut(
            &ferrite_render::DrawingInstruction,
            &ferrite_render::PointOriginGeometry,
        ) -> Result<Option<[f64; 2]>>,
    ) -> Result<Option<PreparedCoverage>> {
        ensure!(
            exempt_ordinals
                .iter()
                .all(|i| *i < context.instruction_count()),
            "Overlay ordinal outside context"
        );
        ensure!(
            view.extent.iter().all(|v| *v > 0),
            "Empty coverage device extent"
        );
        if self.current_dataset_count() == 0 {
            return Ok(None);
        }
        let inventory = self.project_with(project_surfaces)?;
        let plan =
            crate::coverage_loading::display_plan(&inventory, view.display_scale, view.viewport)?;
        let frame = Arc::new(CoverageFrame::new(
            &plan.eligible_inventory,
            &plan.selection,
            view.viewport,
            view.extent,
            view.pixel_budget,
        )?);
        let mut ordinal = 0;
        let pass = PreparedCoveragePass::prepare(
            context.raw_instructions(),
            frame,
            |command| {
                let index = ordinal;
                ordinal += 1;
                if exempt_ordinals.contains(&index) {
                    return Ok(InstructionCoverageClass::Exempt);
                }
                let cell = command.cell_index().ok_or_else(|| {
                    RenderError::Render("Chart command missing dataset identity".into())
                })? as usize;
                let dataset = self.datasets.get(cell).ok_or_else(|| {
                    RenderError::Render("Chart command refers to unknown dataset".into())
                })?;
                Ok(if dataset.is_some() {
                    InstructionCoverageClass::Dataset(cell)
                } else {
                    InstructionCoverageClass::Exempt
                })
            },
            |command, origin| {
                project_origin(command, origin).map_err(|e| RenderError::Render(e.to_string()))
            },
        )?;
        Ok(Some(PreparedCoverage::new(
            context.geometry_revision(),
            context.coverage_view_revision(),
            context.instruction_count(),
            vec![pass],
        )?))
    }
}

#[cfg(test)]
mod projected_view_tests {
    use super::*;
    use crate::coverage_geometry::{DataCoverage, GeographicSurface};
    use ferrite_render::{GeoBounds, PointInstruction, PortrayalOrigin, Viewport};
    fn coverage(key: i64) -> DataCoverage {
        DataCoverage {
            feature_key: key,
            drawing_index: None,
            scales: ferrite_kernel::scale_policy::CoverageScaleRange {
                minimum_denominator: Some(180000),
                optimum_denominator: 90000,
                maximum_denominator: 45000,
            },
            surfaces: vec![GeographicSurface {
                exterior: vec![[0., 0.], [2., 0.], [2., 2.], [0., 2.], [0., 0.]],
                holes: vec![vec![
                    [0.5, 0.5],
                    [1.5, 0.5],
                    [1.5, 1.5],
                    [0.5, 1.5],
                    [0.5, 0.5],
                ]],
            }],
        }
    }
    fn viewport() -> Region {
        Region::from_rings(&[[0., 0.], [32., 0.], [32., 32.], [0., 32.], [0., 0.]], &[]).unwrap()
    }
    fn projected(_: usize, _: i64, surfaces: &[GeographicSurface]) -> Result<Region> {
        let p = |ring: &[[f64; 2]]| {
            ring.iter()
                .map(|p| [8. + 4. * p[1], 16. - 4. * p[0]])
                .collect::<Vec<_>>()
        };
        Region::from_rings(
            &p(&surfaces[0].exterior),
            &surfaces[0].holes.iter().map(|h| p(h)).collect::<Vec<_>>(),
        )
    }
    fn context(cell: usize) -> RenderContext {
        let mut c = RenderContext::new(Viewport::new(32., 32.));
        c.set_bounds(GeoBounds::new(0., 0., 2., 2.));
        let mut command = ferrite_render::DrawingInstruction::Point(
            PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0.25, 0.25))
                .with_cell_index(cell),
        );
        command.set_portrayal_origin(
            PortrayalOrigin::feature_point(WorldPoint::new(0.25, 0.25)).unwrap(),
        );
        c.add_instruction(command);
        c.get_sorted_instructions();
        c
    }
    #[test]
    fn alternate_projection_preserves_component_holes_scales_and_dataset_identity() {
        let inventory = GeographicCoverageInventory {
            datasets: vec![Some(vec![coverage(7)]), None, Some(vec![coverage(9)])],
        };
        let result = inventory.project_with(projected).unwrap();
        assert_eq!(
            result
                .iter()
                .map(|r| (r.dataset_id, r.coverage_id))
                .collect::<Vec<_>>(),
            vec![(0, 7), (2, 9)]
        );
        for r in result {
            assert_eq!(r.region.area(), 48.);
            assert_eq!(r.scales.optimum_denominator, 90000);
            assert_eq!(r.region.polygons()[0].interiors().len(), 1);
        }
        let error = inventory
            .project_with(|_, _, _| anyhow::bail!("horizon projection failed"))
            .unwrap_err();
        assert!(error.to_string().contains("horizon projection failed"));
    }
    #[test]
    fn invisible_origin_is_hidden_and_camera_revision_rejects_old_binding() {
        let source = GeographicCoverageInventory {
            datasets: vec![Some(vec![coverage(7)])],
        };
        let mut c = context(0);
        let v = viewport();
        c.invalidate_coverage_view();
        let prepared = source
            .prepare_projected(
                &c,
                &BTreeSet::new(),
                ProjectedCoverageView {
                    viewport: &v,
                    extent: [32, 32],
                    display_scale: 90000.,
                    pixel_budget: 10000,
                },
                projected,
                |_, _| Ok(None),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            prepared.pass(0).unwrap().decision(0).unwrap(),
            ferrite_kernel::coverage_frame::FrameCoverageDecision::Hidden
        );
        c.set_prepared_coverage(prepared).unwrap();
        assert!(c.prepared_coverage_binding().unwrap().is_some());
        c.invalidate_coverage_view();
        assert!(c.prepared_coverage_binding().is_err());
    }
    #[test]
    fn projected_view_retains_identity_and_memory_fail_closed_contracts() {
        let source = GeographicCoverageInventory {
            datasets: vec![Some(vec![coverage(7)])],
        };
        let v = viewport();
        for (cell, extent, budget, scale) in [
            (99, [32, 32], 10000, 90000.),
            (0, [0, 32], 10000, 90000.),
            (0, [32, 32], 1, 90000.),
            (0, [32, 32], 10000, f64::NAN),
        ] {
            assert!(source
                .prepare_projected(
                    &context(cell),
                    &BTreeSet::new(),
                    ProjectedCoverageView {
                        viewport: &v,
                        extent,
                        display_scale: scale,
                        pixel_budget: budget
                    },
                    projected,
                    |_, _| Ok(Some([9., 15.]))
                )
                .is_err());
        }
        assert!(source
            .prepare_projected(
                &context(0),
                &BTreeSet::from([1]),
                ProjectedCoverageView {
                    viewport: &v,
                    extent: [32, 32],
                    display_scale: 90000.,
                    pixel_budget: 10000
                },
                projected,
                |_, _| Ok(Some([9., 15.]))
            )
            .is_err());
    }
}
