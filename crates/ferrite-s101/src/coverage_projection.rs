//! S-101 coverage inventory projected by the same flat view as chart geometry.
//! Keep extraction outside navigation; device projection and selection are per view.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::coverage_frame::CoverageFrame;
use ferrite_kernel::coverage_selection::{CoverageFootprint, Region};
use ferrite_kernel::{coverage_frame::CoverageSource, coverage_rendering::InstructionOrigin};
use ferrite_render::{
    InstructionCoverageClass, PreparedCoverage, PreparedCoveragePass, RenderContext, RenderError,
    Scaler, WorldPoint,
};
use ferrite_render::{PointOriginGeometry, PortrayalOrigin};
use ferrite_s100_core::S101Cell;
use std::{
    collections::{BTreeSet, HashMap},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
};

#[derive(Debug)]
pub struct GeographicCoverageInventory {
    datasets: Vec<Option<Vec<crate::coverage_geometry::DataCoverage>>>,
    binding_cache: Mutex<Option<Arc<FlatSourceBinding>>>,
    binding_cache_enabled: bool,
    binding_requests: AtomicU64,
    binding_hits: AtomicU64,
}
#[derive(Debug, Clone)]
enum FlatBoundSource {
    Exempt,
    NonPoint(usize),
    Point(usize, Arc<PointOriginGeometry>),
}
/// Default-on qualified northing reuse. Every explicit invalid value fails closed.
/// Read only when creating an immutable source binding; never changes view rights.
fn flat_source_northing_cache_enabled() -> bool {
    let value = std::env::var("FERRITE_FLAT_SOURCE_NORTHING_CACHE");
    flat_source_northing_cache_policy(value.as_deref())
}
fn flat_source_northing_cache_policy(value: Result<&str, &std::env::VarError>) -> bool {
    match value {
        Ok(value) => value == "1",
        Err(std::env::VarError::NotPresent) => true,
        Err(std::env::VarError::NotUnicode(_)) => false,
    }
}

#[derive(Debug)]
struct FlatSourceBinding {
    revision: u64,
    count: usize,
    exemptions: BTreeSet<usize>,
    slots: Vec<usize>,
    sources: Vec<FlatBoundSource>,
    northings: Mutex<Option<Arc<FlatAnchorNorthings>>>,
    northings_enabled: bool,
    northing_requests: AtomicU64,
    northing_hits: AtomicU64,
}
#[derive(Debug)]
struct FlatAnchorNorthings {
    projection: ferrite_render::FlatProjection,
    values: Vec<Option<ferrite_kernel::map_camera::PreparedFlatNorthing>>,
    ready_count: usize,
}
#[derive(Debug, Hash, PartialEq, Eq)]
enum FlatSourceKey {
    Exempt,
    NonPoint(usize),
    Point(usize, usize),
}
impl FlatSourceBinding {
    const MAX_LOGICAL_BYTES: usize = 32 * 1024 * 1024;
    fn admitted(count: usize, exemptions: usize) -> bool {
        count
            .checked_mul(128)
            .and_then(|n| exemptions.checked_mul(32).and_then(|e| n.checked_add(e)))
            .is_some_and(|n| n <= Self::MAX_LOGICAL_BYTES)
    }
    // Logical payload cap, not whole-process RSS. No errors or screen decisions cached.
    const MAX_NORTHING_BYTES: usize = 1024 * 1024;
    fn northings(&self, scaler: &Scaler) -> Option<Arc<FlatAnchorNorthings>> {
        if !self.northings_enabled
            || scaler.projection() != ferrite_render::FlatProjection::EllipsoidalMercator
            || !Self::northing_admitted(self.sources.len())
        {
            return None;
        }
        self.northing_requests.fetch_add(1, Ordering::Relaxed);
        let mut cache = self.northings.lock().ok()?;
        if let Some(value) = cache
            .as_ref()
            .filter(|v| v.projection == scaler.projection())
        {
            self.northing_hits.fetch_add(1, Ordering::Relaxed);
            return Some(value.clone());
        }
        let values: Vec<_> = self
            .sources
            .iter()
            .map(|source| {
                let latitude = match source {
                    FlatBoundSource::Point(_, p) => match p.as_ref() {
                        PointOriginGeometry::FeaturePoint(p) => Some(p.y),
                        PointOriginGeometry::AugmentedPoint {
                            crs: ferrite_render::PointOriginCrs::Geographic,
                            coordinates,
                        } => Some(coordinates[1]),
                        PointOriginGeometry::AugmentedLocalPoint {
                            reference_point, ..
                        } => Some(reference_point.y),
                        _ => None,
                    },
                    _ => None,
                };
                latitude.and_then(|lat| scaler.prepare_flat_northing(lat).ok())
            })
            .collect();
        let ready_count = values.iter().filter(|value| value.is_some()).count();
        let value = Arc::new(FlatAnchorNorthings {
            projection: scaler.projection(),
            values,
            ready_count,
        });
        *cache = Some(value.clone());
        Some(value)
    }
    fn northing_admitted(count: usize) -> bool {
        count
            .checked_mul(64)
            .is_some_and(|n| n <= Self::MAX_NORTHING_BYTES)
    }
    fn matches(&self, context: &RenderContext, exempt: &BTreeSet<usize>) -> bool {
        context.instructions_are_sorted()
            && self.revision == context.geometry_revision()
            && self.count == context.instruction_count()
            && self.exemptions == *exempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::{GeoBounds, PointInstruction, PointOriginCrs, PortrayalOrigin, Viewport};
    fn inventory() -> GeographicCoverageInventory {
        GeographicCoverageInventory {
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
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
    /// A valid ring whose two long edges are 1e-6 degree apart: at high zoom
    /// the -360 degree copy lies millions of pixels away, where f32 merges the
    /// edges into a self-intersection (UKHO 101GB005DEVQH, April 2026).
    #[test]
    fn wrapped_copies_far_from_the_view_keep_full_precision() {
        let mut sliver = inventory();
        let (x, x2) = (-0.9, -0.9 + 1e-6);
        let mut exterior: Vec<[f64; 2]> = (0..=40).map(|i| [x, 50.78 - i as f64 * 5e-4]).collect();
        exterior.extend((0..=40).rev().map(|i| [x2, 50.78 - i as f64 * 5e-4]));
        exterior.push(exterior[0]);
        sliver.datasets[0].as_mut().unwrap()[0].surfaces[0].exterior = exterior;
        let scaler = Scaler::new(
            ferrite_render::GeoBounds::new(-0.95, 50.75, -0.85, 50.79),
            Viewport::new(1420., 862.),
        );
        assert!(scaler.scale_x() > 10_000.);
        let projected = sliver.project(&scaler, true).unwrap();
        // One footprint per coverage: the union of the central and both copies.
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].region.polygons().len(), 3);
    }
    #[test]
    fn cached_slots_preserve_current_view_decisions_and_fragment_rights() {
        let cold = inventory();
        let mut cached = inventory();
        cached.binding_cache_enabled = true;
        let origin = PortrayalOrigin::augmented_point(PointOriginCrs::Portrayal, [5., 5.]).unwrap();
        let mut context = context(Some(0), origin);
        let duplicate = context.raw_instructions()[0].clone();
        context.add_instruction(duplicate);
        let nonpoint =
            PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0.5, 0.5)).with_cell_index(0);
        // A separate primitive with a nonpoint source is kept in its original slot.
        let mut command = ferrite_render::DrawingInstruction::Point(nonpoint);
        command.set_portrayal_origin(PortrayalOrigin::NonPoint);
        context.add_instruction(command);
        context.get_sorted_instructions();
        for dx in [0., 7., -12.] {
            context.scaler.pan(dx, 3.);
            let a = cold
                .prepare_flat(&context, &BTreeSet::new(), [64, 40], true, 10000)
                .unwrap()
                .unwrap();
            let b = cached
                .prepare_flat(&context, &BTreeSet::new(), [64, 40], true, 10000)
                .unwrap()
                .unwrap();
            assert_eq!(a.pass_count(), b.pass_count());
            for pass in 0..a.pass_count() {
                for ordinal in 0..context.instruction_count() {
                    let a = a.pass(pass).unwrap();
                    let b = b.pass(pass).unwrap();
                    assert_eq!(a.decision(ordinal).unwrap(), b.decision(ordinal).unwrap());
                    for pixel in [[0., 0.], [5., 5.], [32., 20.], [63., 39.]] {
                        assert_eq!(
                            a.accepts_fragment(ordinal, pixel).unwrap(),
                            b.accepts_fragment(ordinal, pixel).unwrap()
                        );
                    }
                }
            }
        }
        let (_, requests, hits, slots, sources) = cached.flat_binding_cache_statistics();
        assert_eq!((requests, hits, slots, sources), (3, 2, 3, 2));
    }
    #[test]
    fn exact_owner_exemptions_and_source_mutation_invalidate_binding() {
        let mut inventory = inventory();
        inventory.binding_cache_enabled = true;
        let mut context = context(Some(0), PortrayalOrigin::NonPoint);
        let first = inventory
            .flat_binding(&context, &BTreeSet::new())
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &inventory
                .flat_binding(&context, &BTreeSet::new())
                .unwrap()
                .unwrap()
        ));
        let overlay = inventory
            .flat_binding(&context, &BTreeSet::from([0]))
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &overlay));
        assert!(matches!(overlay.sources[0], FlatBoundSource::Exempt));
        context.set_portrayal_origin_from(
            0,
            PortrayalOrigin::augmented_point(PointOriginCrs::Portrayal, [7., 9.]).unwrap(),
        );
        let changed = inventory
            .flat_binding(&context, &BTreeSet::new())
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &changed));
        assert!(matches!(changed.sources[0], FlatBoundSource::Point(0, _)));
        let separate = self::context(Some(0), PortrayalOrigin::NonPoint);
        assert!(!first.matches(&separate, &BTreeSet::new()));
        context.add_instruction(context.raw_instructions()[0].clone());
        assert!(inventory
            .flat_binding(&context, &BTreeSet::new())
            .unwrap()
            .is_none());
        context.get_sorted_instructions();
        assert_eq!(
            inventory
                .flat_binding(&context, &BTreeSet::new())
                .unwrap()
                .unwrap()
                .slots
                .len(),
            2
        );
    }
    #[test]
    fn cache_never_retains_errors_or_unbounded_bindings() {
        let mut cached = inventory();
        cached.binding_cache_enabled = true;
        for cell in [None, Some(99)] {
            let context = context(cell, PortrayalOrigin::NonPoint);
            assert!(cached.flat_binding(&context, &BTreeSet::new()).is_err());
            assert!(cached.binding_cache.lock().unwrap().is_none());
        }
        assert!(!FlatSourceBinding::admitted(usize::MAX, 0));
        assert!(!FlatSourceBinding::admitted(0, usize::MAX));
        assert!(!FlatSourceBinding::admitted(
            FlatSourceBinding::MAX_LOGICAL_BYTES / 128 + 1,
            0
        ));
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
        {
            let crs = PointOriginCrs::Local;
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
        Ok(Self {
            datasets,
            binding_cache: Mutex::new(None),
            // Exact immutable source binding is reused; screen decisions remain fresh.
            // Explicit 0 retains the uncached diagnostic/control route.
            binding_cache_enabled: std::env::var("FERRITE_FLAT_COVERAGE_BINDING_CACHE").as_deref()
                != Ok("0"),
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
        })
    }

    pub fn current_dataset_count(&self) -> usize {
        self.datasets.iter().filter(|d| d.is_some()).count()
    }

    /// Diagnostic counters only; never a visibility or authorization token.
    pub fn flat_binding_cache_statistics(&self) -> (bool, u64, u64, usize, usize) {
        let (slots, sources) = self
            .binding_cache
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|b| (b.slots.len(), b.sources.len())))
            .unwrap_or_default();
        (
            self.binding_cache_enabled,
            self.binding_requests.load(Ordering::Relaxed),
            self.binding_hits.load(Ordering::Relaxed),
            slots,
            sources,
        )
    }
    /// Diagnostic only; no geometry/visibility authorization and no frame allocation.
    pub fn flat_northing_cache_statistics(&self) -> (bool, u64, u64, usize, usize) {
        let Ok(cache) = self.binding_cache.lock() else {
            return (false, 0, 0, 0, 0);
        };
        let Some(binding) = cache.as_ref() else {
            return (false, 0, 0, 0, 0);
        };
        let retained = binding
            .northings
            .lock()
            .ok()
            .and_then(|value| {
                value
                    .as_ref()
                    .map(|value| (value.values.len(), value.ready_count))
            })
            .unwrap_or((0, 0));
        (
            binding.northings_enabled,
            binding.northing_requests.load(Ordering::Relaxed),
            binding.northing_hits.load(Ordering::Relaxed),
            retained.0,
            retained.1,
        )
    }
    fn build_flat_binding(
        &self,
        context: &RenderContext,
        exempt: &BTreeSet<usize>,
    ) -> Result<FlatSourceBinding> {
        let mut sources = Vec::new();
        let mut slots = Vec::with_capacity(context.instruction_count());
        let mut lookup = HashMap::new();
        for (ordinal, command) in context.raw_instructions().iter().enumerate() {
            let (key, source) = if exempt.contains(&ordinal) {
                (FlatSourceKey::Exempt, FlatBoundSource::Exempt)
            } else {
                let cell = command.cell_index().ok_or_else(|| {
                    RenderError::Render("Chart command missing dataset identity".into())
                })? as usize;
                let dataset = self.datasets.get(cell).ok_or_else(|| {
                    RenderError::Render("Chart command refers to unknown dataset".into())
                })?;
                if dataset.is_none() {
                    (FlatSourceKey::Exempt, FlatBoundSource::Exempt)
                } else {
                    match command.portrayal_origin() {
                        PortrayalOrigin::CoverageExempt => {
                            return Err(RenderError::Render(
                                "Host overlay cannot be assigned a product dataset".into(),
                            )
                            .into())
                        }
                        PortrayalOrigin::Unspecified => {
                            return Err(RenderError::Render(
                                "Missing portrayal origin for coverage binding".into(),
                            )
                            .into())
                        }
                        PortrayalOrigin::NonPoint => (
                            FlatSourceKey::NonPoint(cell),
                            FlatBoundSource::NonPoint(cell),
                        ),
                        PortrayalOrigin::Point(p) => (
                            FlatSourceKey::Point(cell, Arc::as_ptr(p) as usize),
                            FlatBoundSource::Point(cell, p.clone()),
                        ),
                    }
                }
            };
            let slot = *lookup.entry(key).or_insert_with(|| {
                let i = sources.len();
                sources.push(source);
                i
            });
            slots.push(slot);
        }
        Ok(FlatSourceBinding {
            revision: context.geometry_revision(),
            count: context.instruction_count(),
            exemptions: exempt.clone(),
            slots,
            sources,
            northings: Mutex::new(None),
            northings_enabled: flat_source_northing_cache_enabled(),
            northing_requests: AtomicU64::new(0),
            northing_hits: AtomicU64::new(0),
        })
    }
    fn flat_binding(
        &self,
        context: &RenderContext,
        exempt: &BTreeSet<usize>,
    ) -> Result<Option<Arc<FlatSourceBinding>>> {
        if !self.binding_cache_enabled
            || !context.instructions_are_sorted()
            || !FlatSourceBinding::admitted(context.instruction_count(), exempt.len())
        {
            return Ok(None);
        }
        self.binding_requests.fetch_add(1, Ordering::Relaxed);
        let Ok(mut cache) = self.binding_cache.lock() else {
            return Ok(None);
        };
        if cache.as_ref().is_some_and(|b| b.matches(context, exempt)) {
            self.binding_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(cache.as_ref().cloned());
        }
        let binding = self.build_flat_binding(context, exempt)?;
        // Store an Arc once, not a clone of every source/slot on each camera.
        let shared = Arc::new(binding);
        *cache = Some(shared.clone());
        drop(cache);
        Ok(Some(shared))
    }
    /// All longitude copies participate in one selection in device coordinates.
    /// A dataset retains one identity even when its footprint crosses the seam.
    pub fn project(&self, scaler: &Scaler, wrapping: bool) -> Result<Vec<CoverageFootprint>> {
        let mut inventory = Vec::new();
        for (dataset_id, coverages) in self.datasets.iter().enumerate() {
            let Some(coverages) = coverages else { continue };
            for coverage in coverages {
                let mut region: Option<Region> = None;
                for (surface_index, surface) in coverage.surfaces.iter().enumerate() {
                    for &shift in &[0., -360., 360.][..if wrapping { 3 } else { 1 }] {
                        let project = |ring: &[[f64; 2]]| -> Result<Vec<[f64; 2]>> {
                            ring.iter()
                                .map(|p| {
                                    // f64 throughout: a ±360° copy lies ~10^5–10^6 px away,
                                    // where f32 rounding merges distinct boundary points and
                                    // can turn a valid ring self-intersecting.
                                    let p = scaler
                                        .world_to_screen_f64(WorldPoint::new(p[0] + shift, p[1]));
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
                        let part = Region::from_rings(&exterior, &holes).with_context(|| {
                            // Error-only bounded evidence. Preserve the original projection,
                            // validation order, ring data and rejection; never skip a component.
                            let consecutive = 1 + exterior.windows(2).filter(|p| p[0] != p[1]).count();
                            format!(
                                "Coverage surface projection failed: dataset={dataset_id} coverage={} surface={surface_index} longitude_shift={shift} original_exterior_len={} projected_exterior_len={} consecutive_distinct={} holes={} original_first4={:?} projected_first4={:?} viewport={:?} scale_xy={:?} offset_xy={:?}",
                                coverage.feature_key, surface.exterior.len(), exterior.len(),
                                if exterior.is_empty() { 0 } else { consecutive }, holes.len(),
                                &surface.exterior[..surface.exterior.len().min(4)],
                                &exterior[..exterior.len().min(4)], scaler.viewport,
                                [scaler.scale_x(), scaler.scale_y()],
                                [scaler.offset_x(), scaler.offset_y()]
                            )
                        })?;
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
        let binding = self.flat_binding(context, exempt_ordinals)?;
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
        ).with_context(|| format!(
            "Coverage viewport projection failed: viewport={v:?} rounded_edges={:?} edge_bits={:?} physical_extent={extent:?} scale_xy={:?} offset_xy={:?}",
            [x, y, right, bottom], [x, y, right, bottom].map(f64::to_bits),
            [scaler.scale_x(), scaler.scale_y()], [scaler.offset_x(), scaler.offset_y()]
        ))?;
        let plan =
            crate::coverage_loading::display_plan(&inventory, scaler.display_scale, &viewport)?;
        let frame = Arc::new(CoverageFrame::new_with_scale_annotations(
            &plan.eligible_inventory,
            &plan.selection,
            &viewport,
            extent,
            pixel_budget,
            scaler.display_scale,
            [
                v.x as f64 + v.width as f64 * 0.5,
                v.y as f64 + v.height as f64 * 0.5,
            ],
        )?);
        // PreparedCoverage and its caller share the already-sorted raw order.
        let northings = binding
            .as_ref()
            .and_then(|binding| binding.northings(scaler));
        let mut passes = Vec::new();
        for &shift in &[0., -360., 360.][..if wrapping { 3 } else { 1 }] {
            if let Some(binding) = binding.as_ref() {
                // Project unique immutable source metadata in THIS actual view.
                let sources = binding
                    .sources
                    .iter()
                    .enumerate()
                    .map(
                        |(source_index, source)| -> ferrite_render::Result<(CoverageSource, bool)> {
                            Ok(match source {
                                FlatBoundSource::Exempt => (CoverageSource::Exempt, false),
                                FlatBoundSource::NonPoint(dataset_id) => (
                                    CoverageSource::Dataset {
                                        dataset_id: *dataset_id,
                                        origin: InstructionOrigin::NonPoint,
                                    },
                                    false,
                                ),
                                FlatBoundSource::Point(dataset_id, origin) => {
                                    match PortrayalOrigin::project_flat_source_with_northing(
                                        origin,
                                        scaler,
                                        shift,
                                        northings
                                            .as_ref()
                                            .and_then(|n| n.values[source_index].as_ref()),
                                    )? {
                                        Some(p) => (
                                            CoverageSource::Dataset {
                                                dataset_id: *dataset_id,
                                                origin: InstructionOrigin::Point([
                                                    p.x as f64, p.y as f64,
                                                ]),
                                            },
                                            false,
                                        ),
                                        None => (
                                            CoverageSource::Dataset {
                                                dataset_id: *dataset_id,
                                                origin: InstructionOrigin::NonPoint,
                                            },
                                            true,
                                        ),
                                    }
                                }
                            })
                        },
                    )
                    .collect::<ferrite_render::Result<Vec<_>>>()?;
                passes.push(PreparedCoveragePass::prepare_source_slots(
                    frame.clone(),
                    &sources,
                    &binding.slots,
                )?);
                continue;
            }
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
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
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
/// drawing/picking passes. A projection caller invalidates the context coverage view
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
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
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
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
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
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
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

#[cfg(test)]
mod northing_cache_contract_tests {
    use super::*;
    #[test]
    fn default_on_and_explicit_opt_out_policy_are_exact() {
        assert!(flat_source_northing_cache_policy(Err(
            &std::env::VarError::NotPresent
        )));
        assert!(flat_source_northing_cache_policy(Ok("1")));
        for value in ["0", "", "true", "false", "2", "01", " 1", "1 ", "1\n"] {
            assert!(!flat_source_northing_cache_policy(Ok(value)), "{value:?}");
        }
    }
    #[test]
    fn non_unicode_failure_is_distinct_from_absent_default() {
        // Pure input: no shared process environment mutation or OS-specific test race.
        let invalid = std::env::VarError::NotUnicode(std::ffi::OsString::from("invalid marker"));
        assert!(!flat_source_northing_cache_policy(Err(&invalid)));
        assert!(flat_source_northing_cache_policy(Err(
            &std::env::VarError::NotPresent
        )));
        assert!(!flat_source_northing_cache_policy(Ok("0")));
    }

    fn binding(latitude: f64, dataset: usize) -> FlatSourceBinding {
        FlatSourceBinding {
            revision: 1,
            count: 1,
            exemptions: BTreeSet::new(),
            slots: vec![0],
            sources: vec![FlatBoundSource::Point(
                dataset,
                Arc::new(PointOriginGeometry::FeaturePoint(WorldPoint::new(
                    179., latitude,
                ))),
            )],
            northings: Mutex::new(None),
            northings_enabled: true,
            northing_requests: AtomicU64::new(0),
            northing_hits: AtomicU64::new(0),
        }
    }
    #[test]
    fn owner_source_projection_and_decline_contracts() {
        let mut scaler =
            ferrite_render::RenderContext::new(ferrite_render::Viewport::new(64., 40.)).scaler;
        scaler.set_bounds(ferrite_render::GeoBounds::new(-10., 30., 10., 60.));
        scaler.set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
        let first = binding(48.65, 0);
        let a = first.northings(&scaler).unwrap();
        let b = first.northings(&scaler).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(first.northing_hits.load(Ordering::Relaxed), 1);
        let next = binding(48.66, 1);
        let c = next.northings(&scaler).unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        scaler.set_bounds(ferrite_render::GeoBounds::new(-10., 30., 10., 60.));
        assert!(Arc::ptr_eq(&a, &first.northings(&scaler).unwrap()));
        scaler.set_projection(ferrite_render::FlatProjection::LocalGeographic);
        assert!(first.northings(&scaler).is_none());
        scaler.set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
        let invalid = binding(91., 0).northings(&scaler).unwrap();
        assert!(invalid.values[0].is_none()); // Legacy conversion still runs; no cached error.
        let mut off = binding(48.65, 0);
        off.northings_enabled = false;
        assert!(off.northings(&scaler).is_none());
        assert!(off.northings.lock().unwrap().is_none());
    }
    #[test]
    fn whole_overcap_binding_declines_without_cached_prefix() {
        let mut scaler =
            ferrite_render::RenderContext::new(ferrite_render::Viewport::new(64., 40.)).scaler;
        scaler.set_bounds(ferrite_render::GeoBounds::new(-10., 30., 10., 60.));
        scaler.set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
        let mut source = binding(48.65, 0);
        source
            .sources
            .resize_with(16385, || FlatBoundSource::NonPoint(0));
        assert!(source.northings(&scaler).is_none());
        assert!(source.northings.lock().unwrap().is_none());
        assert_eq!(source.northing_requests.load(Ordering::Relaxed), 0);
        let FlatBoundSource::Point(_, origin) = &source.sources[0] else {
            panic!();
        };
        let old = PortrayalOrigin::project_flat_source(origin, &scaler, 0.)
            .unwrap()
            .unwrap();
        let next = PortrayalOrigin::project_flat_source_with_northing(origin, &scaler, 0., None)
            .unwrap()
            .unwrap();
        assert_eq!(
            [old.x.to_bits(), old.y.to_bits()],
            [next.x.to_bits(), next.y.to_bits()]
        );
    }
    #[test]
    fn cap_precedes_allocation_and_overflow_falls_back() {
        assert!(FlatSourceBinding::northing_admitted(16384));
        assert!(!FlatSourceBinding::northing_admitted(16385));
        assert!(!FlatSourceBinding::northing_admitted(usize::MAX));
    }
}
