//! S-101 coverage inventory projected by the same flat view as chart geometry.
//! Keep extraction outside navigation; device projection and selection are per view.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::coverage_frame::CoverageFrame;
use ferrite_kernel::coverage_selection::{CoverageFootprint, Region};
use ferrite_kernel::{coverage_frame::CoverageSource, coverage_rendering::InstructionOrigin};
#[cfg(test)]
use ferrite_render::WorldPoint;
use ferrite_render::{
    InstructionCoverageClass, PreparedCoverage, PreparedCoveragePass, RenderContext, RenderError,
    Scaler,
};
use ferrite_render::{PointOriginGeometry, PortrayalOrigin};
use ferrite_s100_core::S101Cell;
use std::{
    collections::BTreeSet,
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
    /// Validated coverage regions in the camera-independent (longitude,
    /// northing) plane, keyed by plane identity and wrapping.
    plane_cache: Mutex<Option<Arc<PlaneCoverage>>>,
}
/// Retained region per coverage index (`coverage_loading::retention`).
type Retention = Vec<(usize, Region)>;
#[derive(Debug)]
struct PlaneCoverage {
    key: ([u64; 3], bool),
    footprints: Vec<CoverageFootprint>,
    /// 4.6 retention in the plane, keyed by `retention_key`.
    retention: Mutex<Option<(usize, Arc<Retention>)>>,
}
impl PlaneCoverage {
    fn retention(&self, denominator: f64) -> Arc<Retention> {
        let key = crate::coverage_loading::retention_key(&self.footprints, denominator);
        let mut cache = self.retention.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, value)) = cache.as_ref().filter(|(k, _)| *k == key) {
            return value.clone();
        }
        let value = Arc::new(crate::coverage_loading::retention(
            &self.footprints,
            denominator,
        ));
        *cache = Some((key, value.clone()));
        value
    }
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
    planes: Mutex<Option<Arc<FlatSourcePlanes>>>,
}
/// Camera-independent (longitude, northing) of each geographic point source;
/// NaN marks sources that keep the general resolver (local/portrayal CRS).
#[derive(Debug)]
struct FlatSourcePlanes {
    identity: [u64; 3],
    values: Vec<[f64; 2]>,
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
    // 128 B per instruction is a generous logical bound (actual use is ~50 B,
    // under the 280 B each instruction already occupies). 512 MiB admits ~4M
    // instructions; a 32 MiB cap disabled the binding at ~262k instructions,
    // i.e. around 30 ENC cells, and every scene then reprojected all sources.
    const MAX_LOGICAL_BYTES: usize = 512 * 1024 * 1024;
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
    /// 16 bytes per source, bounded by the binding's own admission budget.
    fn planes(
        &self,
        affine: &ferrite_kernel::map_camera::FlatAffine,
    ) -> Option<Arc<FlatSourcePlanes>> {
        if !self.northings_enabled {
            return None;
        }
        let mut cache = self.planes.lock().ok()?;
        if let Some(value) = cache
            .as_ref()
            .filter(|v| v.identity == affine.plane_identity())
        {
            return Some(value.clone());
        }
        let values = self
            .sources
            .iter()
            .map(|source| {
                let point = match source {
                    FlatBoundSource::Point(_, p) => match p.as_ref() {
                        PointOriginGeometry::FeaturePoint(p) => Some([p.x, p.y]),
                        PointOriginGeometry::AugmentedPoint {
                            crs: ferrite_render::PointOriginCrs::Geographic,
                            coordinates,
                        } => Some(*coordinates),
                        _ => None,
                    },
                    _ => None,
                };
                point
                    .and_then(|p| affine.plane(p).ok())
                    .unwrap_or([f64::NAN; 2])
            })
            .collect();
        let value = Arc::new(FlatSourcePlanes {
            identity: affine.plane_identity(),
            values,
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
            plane_cache: Mutex::new(None),
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
            plane_cache: Mutex::new(None),
        })
    }

    /// S-101 2.0.0 4.6: the finest optimum display scale denominator among
    /// Data Coverage areas containing `[longitude, latitude]` that is larger
    /// scale than the mariner's selected viewing scale. Some(_) requires the
    /// "larger scale data available" indication for own ship.
    pub fn larger_scale_at(&self, position: [f64; 2], viewing_denominator: f64) -> Option<u32> {
        self.datasets
            .iter()
            .flatten()
            .flatten()
            .filter(|c| (c.scales.optimum_denominator as f64) < viewing_denominator)
            .filter(|c| c.surfaces.iter().any(|s| surface_contains(s, position)))
            .map(|c| c.scales.optimum_denominator)
            .min()
    }

    /// Datasets whose instructions can appear in the view grown by `margin`
    /// (fraction of the viewport on each side) at the current display scale:
    /// those with S-98 Appendix E / 4.6 selected coverage there, and every
    /// dataset without a current coverage model. Unselected datasets are fully
    /// hidden by coverage, so a scene without them draws the same pixels.
    pub fn visible_datasets(
        &self,
        scaler: &Scaler,
        wrapping: bool,
        margin: f64,
    ) -> Result<Vec<bool>> {
        let mut visible: Vec<bool> = self.datasets.iter().map(Option::is_none).collect();
        if self.current_dataset_count() == 0 {
            return Ok(visible);
        }
        let inventory = self.project(scaler, wrapping)?;
        let v = scaler.viewport;
        let (mx, my) = (v.width as f64 * margin, v.height as f64 * margin);
        let [x, y, right, bottom] = [
            v.x as f64 - mx,
            v.y as f64 - my,
            (v.x + v.width) as f64 + mx,
            (v.y + v.height) as f64 + my,
        ];
        let viewport = Region::from_rings(
            &[[x, y], [right, y], [right, bottom], [x, bottom], [x, y]],
            &[],
        )?;
        let affine = scaler.flat_affine();
        let retained = self
            .plane_coverage(&affine, wrapping)?
            .retention(scaler.display_scale)
            .iter()
            .map(|(index, region)| Ok((*index, region.map_affine(|p| affine.screen(p))?)))
            .collect::<Result<Vec<_>>>()?;
        let plan = crate::coverage_loading::display_plan_with_retention(
            &inventory,
            scaler.display_scale,
            &viewport,
            &retained,
        )?;
        for selected in &plan.selection.coverages {
            if let Some(slot) = visible.get_mut(inventory[selected.inventory_index].dataset_id) {
                *slot = true;
            }
        }
        Ok(visible)
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
        // Slots are numbered in first-seen order. Exempt and per-dataset
        // non-point keys are dense, so only point anchors need a hash map.
        let mut exempt_slot = None;
        let mut non_point_slots: Vec<Option<usize>> = vec![None; self.datasets.len()];
        let mut point_slots: rustc_hash::FxHashMap<(usize, usize), usize> =
            rustc_hash::FxHashMap::default();
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
            let entry = match key {
                FlatSourceKey::Exempt => &mut exempt_slot,
                FlatSourceKey::NonPoint(cell) => &mut non_point_slots[cell],
                FlatSourceKey::Point(cell, anchor) => {
                    let next = sources.len();
                    let slot = *point_slots.entry((cell, anchor)).or_insert(next);
                    if slot == next {
                        sources.push(source);
                    }
                    slots.push(slot);
                    continue;
                }
            };
            let slot = *entry.get_or_insert_with(|| {
                sources.push(source);
                sources.len() - 1
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
            planes: Mutex::new(None),
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
        // Validation and union are camera-independent: do them once in the
        // (longitude, northing) plane, then apply this camera's exact affine.
        let affine = scaler.flat_affine();
        let plane = self.plane_coverage(&affine, wrapping)?;
        plane
            .footprints
            .iter()
            .map(|footprint| {
                let region = footprint
                    .region
                    .map_affine(|p| affine.screen(p))
                    .with_context(|| {
                        format!(
                            "Coverage surface projection failed: dataset={} coverage={} viewport={:?} scale_xy={:?} offset_xy={:?}",
                            footprint.dataset_id,
                            footprint.coverage_id,
                            scaler.viewport,
                            [scaler.scale_x(), scaler.scale_y()],
                            [scaler.offset_x(), scaler.offset_y()]
                        )
                    })?;
                Ok(CoverageFootprint {
                    region,
                    ..footprint.clone()
                })
            })
            .collect()
    }

    fn plane_coverage(
        &self,
        affine: &ferrite_kernel::map_camera::FlatAffine,
        wrapping: bool,
    ) -> Result<Arc<PlaneCoverage>> {
        let key = (affine.plane_identity(), wrapping);
        let mut cache = self.plane_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.as_ref().filter(|c| c.key == key) {
            return Ok(cached.clone());
        }
        // Each coverage's region is independent: build them in parallel, then
        // keep dataset/coverage order (and the first error in that order).
        let jobs: Vec<_> = self
            .datasets
            .iter()
            .enumerate()
            .filter_map(|(dataset_id, coverages)| Some((dataset_id, coverages.as_ref()?)))
            .flat_map(|(dataset_id, coverages)| coverages.iter().map(move |c| (dataset_id, c)))
            .collect();
        let footprints = {
            use rayon::prelude::*;
            jobs.par_iter()
                .map(|&(dataset_id, coverage)| -> Result<_> {
                let mut region: Option<Region> = None;
                for (surface_index, surface) in coverage.surfaces.iter().enumerate() {
                    for &shift in &[0., -360., 360.][..if wrapping { 3 } else { 1 }] {
                        let project = |ring: &[[f64; 2]]| -> Result<Vec<[f64; 2]>> {
                            // Same longitude copy as the screen path: p[0] + shift.
                            ring.iter()
                                .map(|p| affine.plane([p[0] + shift, p[1]]))
                                .collect()
                        };
                        let exterior = project(&surface.exterior)?;
                        let holes = surface
                            .holes
                            .iter()
                            .map(|ring| project(ring))
                            .collect::<Result<Vec<_>>>()?;
                        let part = Region::from_rings(&exterior, &holes).with_context(|| {
                            format!(
                                "Coverage surface projection failed: dataset={dataset_id} coverage={} surface={surface_index} longitude_shift={shift} original_exterior_len={} holes={} original_first4={:?}",
                                coverage.feature_key,
                                surface.exterior.len(),
                                holes.len(),
                                &surface.exterior[..surface.exterior.len().min(4)],
                            )
                        })?;
                        region = Some(match region {
                            Some(r) => r.union(&part),
                            None => part,
                        });
                    }
                }
                Ok(CoverageFootprint {
                    dataset_id,
                    coverage_id: coverage.feature_key,
                    scales: coverage.scales,
                    region: region.context("Empty S-101 DataCoverage surface")?,
                })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .collect::<Result<Vec<_>>>()?
        };
        let plane = Arc::new(PlaneCoverage {
            key,
            footprints,
            retention: Mutex::new(None),
        });
        *cache = Some(plane.clone());
        Ok(plane)
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
        // Retention differences are camera-independent; reuse them from the
        // plane and apply only this camera's affine.
        let affine = scaler.flat_affine();
        let retained = self
            .plane_coverage(&affine, wrapping)?
            .retention(scaler.display_scale)
            .iter()
            .map(|(index, region)| Ok((*index, region.map_affine(|p| affine.screen(p))?)))
            .collect::<Result<Vec<_>>>()?;
        let plan = crate::coverage_loading::display_plan_with_retention(
            &inventory,
            scaler.display_scale,
            &viewport,
            &retained,
        )?;
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
        let planes = binding.as_ref().and_then(|binding| binding.planes(&affine));
        let shifts = &[0., -360., 360.][..if wrapping { 3 } else { 1 }];
        let mut passes = Vec::new();
        if let Some(binding) = binding.as_ref() {
            // The longitude copies are independent: prepare them in parallel
            // and keep their order (and the first error in that order).
            use rayon::prelude::*;
            let prepared: Vec<Result<_>> = shifts
                .par_iter()
                .map(|&shift| -> Result<_> {
                    // Project unique immutable source metadata in THIS actual view.
                    let sources = binding
                    .sources
                    .par_iter()
                    .with_min_len(4096)
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
                                    // Geographic points: the same affine and f32
                                    // rounding as the resolver below, without
                                    // re-evaluating Mercator per point and copy.
                                    let fast = planes.as_ref().and_then(|planes| {
                                        let [x, northing] = planes.values[source_index];
                                        if !x.is_finite() {
                                            return None;
                                        }
                                        let p = affine.screen([x + shift, northing]);
                                        let p = [p[0] as f32, p[1] as f32];
                                        (p[0].is_finite() && p[1].is_finite()).then_some(p)
                                    });
                                    if let Some(p) = fast {
                                        return Ok((
                                            CoverageSource::Dataset {
                                                dataset_id: *dataset_id,
                                                origin: InstructionOrigin::Point([
                                                    p[0] as f64,
                                                    p[1] as f64,
                                                ]),
                                            },
                                            false,
                                        ));
                                    }
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
                    // In order, so the first failing source is reported.
                    .collect::<Vec<_>>()
                    .into_iter()
                    .collect::<ferrite_render::Result<Vec<_>>>()?;
                    Ok(PreparedCoveragePass::prepare_source_slots(
                        frame.clone(),
                        &sources,
                        &binding.slots,
                    )?)
                })
                .collect();
            for pass in prepared {
                passes.push(pass?);
            }
        }
        // Without a binding, each pass classifies every instruction in order.
        for &shift in shifts.iter().filter(|_| binding.is_none()) {
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
            plane_cache: Mutex::new(None),
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
            plane_cache: Mutex::new(None),
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
    /// Only datasets whose selected coverage meets the grown view are visible;
    /// a dataset without a current coverage model always is.
    #[test]
    fn visible_datasets_keep_nearby_selected_and_unmodelled_datasets() {
        let near = coverage(7);
        let mut far = coverage(9);
        for p in &mut far.surfaces[0].exterior {
            p[0] += 40.;
        }
        for p in &mut far.surfaces[0].holes[0] {
            p[0] += 40.;
        }
        let inventory = GeographicCoverageInventory {
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
            plane_cache: Mutex::new(None),
            datasets: vec![Some(vec![near]), Some(vec![far]), None],
        };
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.set_bounds(GeoBounds::new(0., 0., 2., 2.));
        let visible = inventory.visible_datasets(&c.scaler, true, 1.0).unwrap();
        assert_eq!(visible, vec![true, false, true]);
        c.set_bounds(GeoBounds::new(-1., -1., 45., 3.));
        let visible = inventory.visible_datasets(&c.scaler, true, 1.0).unwrap();
        assert!(
            visible[1],
            "far dataset is selected once the view reaches it"
        );
    }
    /// The plane cache must match validating and unioning every surface in
    /// screen space, across projections, zooms, pans and longitude copies.
    #[test]
    fn plane_cache_matches_screen_space_validation_and_union() {
        let mut touching = coverage(7);
        touching.surfaces.push(GeographicSurface {
            exterior: vec![[2., 0.], [3., 0.], [3., 1.], [2., 1.], [2., 0.]],
            holes: vec![],
        });
        let mut antimeridian = coverage(9);
        for p in &mut antimeridian.surfaces[0].exterior {
            p[0] += 178.;
        }
        for p in &mut antimeridian.surfaces[0].holes[0] {
            p[0] += 178.;
        }
        let inventory = GeographicCoverageInventory {
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
            plane_cache: Mutex::new(None),
            datasets: vec![Some(vec![touching, antimeridian])],
        };
        let screen_space = |scaler: &Scaler, wrapping: bool| -> Vec<Region> {
            let mut out = Vec::new();
            for coverage in inventory.datasets[0].as_ref().unwrap() {
                let mut region: Option<Region> = None;
                for surface in &coverage.surfaces {
                    for &shift in &[0., -360., 360.][..if wrapping { 3 } else { 1 }] {
                        let p = |ring: &[[f64; 2]]| {
                            ring.iter()
                                .map(|p| {
                                    scaler.world_to_screen_f64(WorldPoint::new(p[0] + shift, p[1]))
                                })
                                .collect::<Vec<_>>()
                        };
                        let holes = surface.holes.iter().map(|h| p(h)).collect::<Vec<_>>();
                        let part = Region::from_rings(&p(&surface.exterior), &holes).unwrap();
                        region = Some(match region {
                            Some(r) => r.union(&part),
                            None => part,
                        });
                    }
                }
                out.push(region.unwrap());
            }
            out
        };
        for projection in [
            ferrite_render::FlatProjection::LocalGeographic,
            ferrite_render::FlatProjection::EllipsoidalMercator,
        ] {
            for (bounds, wrapping) in [
                (GeoBounds::new(0., 0., 3., 2.), false),
                (GeoBounds::new(-1.5, -0.5, 0.75, 1.25), true),
                (GeoBounds::new(179., 0.5, 181., 1.5), true),
            ] {
                let mut c = RenderContext::new(Viewport::new(800., 600.));
                c.scaler.set_projection(projection);
                c.set_bounds(bounds);
                let cached = inventory.project(&c.scaler, wrapping).unwrap();
                let expected = screen_space(&c.scaler, wrapping);
                assert_eq!(cached.len(), expected.len());
                let copies = if wrapping { 3. } else { 1. };
                for ((got, want), coverage) in cached
                    .iter()
                    .zip(&expected)
                    .zip(inventory.datasets[0].as_ref().unwrap())
                {
                    // Surfaces only touch, so the exact union area is the sum of
                    // the projected ring areas. Overlay snaps to a grid relative
                    // to the region extent in either space; the plane result must
                    // be as close to that exact area as the screen-space result.
                    let ring = |r: &[[f64; 2]]| {
                        let p: Vec<_> = r
                            .iter()
                            .map(|p| c.scaler.world_to_screen_f64(WorldPoint::new(p[0], p[1])))
                            .collect();
                        p.windows(2)
                            .map(|w| w[0][0] * w[1][1] - w[1][0] * w[0][1])
                            .sum::<f64>()
                            .abs()
                            / 2.
                    };
                    let exact = copies
                        * coverage
                            .surfaces
                            .iter()
                            .map(|s| {
                                ring(&s.exterior) - s.holes.iter().map(|h| ring(h)).sum::<f64>()
                            })
                            .sum::<f64>();
                    let got_error = (got.region.area() - exact).abs();
                    let want_error = (want.area() - exact).abs();
                    let difference =
                        got.region.difference(want).area() + want.difference(&got.region).area();
                    assert!(
                        got_error <= (2. * want_error).max(exact * 1e-9),
                        "{projection:?} {bounds:?}: plane error {got_error} vs screen error {want_error}"
                    );
                    assert!(
                        difference <= exact * 1e-6,
                        "{projection:?} {bounds:?}: {difference}"
                    );
                    // Screen-space snapping can leave touching surfaces of a far
                    // longitude copy unmerged; the plane union never splits more.
                    assert!(got.region.polygons().len() <= want.polygons().len());
                }
            }
        }
        // One plane build serves every camera of the same projection.
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.set_bounds(GeoBounds::new(0., 0., 3., 2.));
        let first = inventory
            .plane_coverage(&c.scaler.flat_affine(), true)
            .unwrap();
        c.set_bounds(GeoBounds::new(1., 0.5, 2., 1.5));
        let second = inventory
            .plane_coverage(&c.scaler.flat_affine(), true)
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }
    #[test]
    fn invisible_origin_is_hidden_and_camera_revision_rejects_old_binding() {
        let source = GeographicCoverageInventory {
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
            plane_cache: Mutex::new(None),
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
            plane_cache: Mutex::new(None),
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
            planes: Mutex::new(None),
        }
    }
    /// The plane fast path must produce the resolver's exact f32 screen bits.
    #[test]
    fn plane_fast_path_matches_resolver_bits() {
        use ferrite_render::{GeoBounds, Viewport};
        let binding = binding(48.65, 0);
        for projection in [
            ferrite_render::FlatProjection::LocalGeographic,
            ferrite_render::FlatProjection::EllipsoidalMercator,
        ] {
            for bounds in [
                GeoBounds::new(178.5, 48., 179.5, 49.),
                GeoBounds::new(-181.2, 48.6, -180.9, 48.7),
                GeoBounds::new(170., 40., 190., 55.),
            ] {
                let mut c = RenderContext::new(Viewport::new(1280., 720.));
                c.scaler.set_projection(projection);
                c.set_bounds(bounds);
                let affine = c.scaler.flat_affine();
                let planes = binding.planes(&affine).unwrap();
                let FlatBoundSource::Point(_, origin) = &binding.sources[0] else {
                    unreachable!()
                };
                for shift in [0., -360., 360.] {
                    let want = PortrayalOrigin::project_flat_source_with_northing(
                        origin, &c.scaler, shift, None,
                    )
                    .unwrap()
                    .unwrap();
                    let [x, northing] = planes.values[0];
                    let got = affine.screen([x + shift, northing]);
                    assert_eq!(
                        [got[0] as f32, got[1] as f32].map(f32::to_bits),
                        [want.x, want.y].map(f32::to_bits),
                        "{projection:?} {bounds:?} {shift}"
                    );
                }
            }
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

fn ring_contains(ring: &[[f64; 2]], [x, y]: [f64; 2]) -> bool {
    let mut inside = false;
    for edge in ring.windows(2) {
        let ([x0, y0], [x1, y1]) = (edge[0], edge[1]);
        if (y0 > y) != (y1 > y) && x < x0 + (y - y0) * (x1 - x0) / (y1 - y0) {
            inside = !inside;
        }
    }
    inside
}
/// Rings are closed WGS84 longitude/latitude polygons; holes exclude.
fn surface_contains(s: &crate::coverage_geometry::GeographicSurface, p: [f64; 2]) -> bool {
    ring_contains(&s.exterior, p) && !s.holes.iter().any(|h| ring_contains(h, p))
}
#[cfg(test)]
mod larger_scale_tests {
    use super::*;
    use crate::coverage_geometry::{DataCoverage, GeographicSurface};
    use ferrite_kernel::scale_policy::CoverageScaleRange;
    fn square(x: f64, size: f64) -> Vec<[f64; 2]> {
        vec![
            [x, 0.],
            [x + size, 0.],
            [x + size, size],
            [x, size],
            [x, 0.],
        ]
    }
    fn coverage(optimum: u32, exterior: Vec<[f64; 2]>, holes: Vec<Vec<[f64; 2]>>) -> DataCoverage {
        DataCoverage {
            feature_key: optimum as i64,
            scales: CoverageScaleRange {
                minimum_denominator: None,
                optimum_denominator: optimum,
                maximum_denominator: optimum / 2,
            },
            drawing_index: None,
            surfaces: vec![GeographicSurface { exterior, holes }],
        }
    }
    #[test]
    fn own_ship_indication_reports_the_finest_larger_scale_coverage_at_the_position() {
        let inventory = GeographicCoverageInventory {
            datasets: vec![
                Some(vec![coverage(90_000, square(0., 10.), vec![])]),
                Some(vec![coverage(22_000, square(0., 4.), vec![square(1., 1.)])]),
                None,
            ],
            binding_cache: Mutex::new(None),
            binding_cache_enabled: false,
            binding_requests: AtomicU64::new(0),
            binding_hits: AtomicU64::new(0),
            plane_cache: Mutex::new(None),
        };
        assert_eq!(inventory.larger_scale_at([3., 3.], 180_000.), Some(22_000));
        // The MSVS already shows the finer data: no indication.
        assert_eq!(inventory.larger_scale_at([3., 3.], 22_000.), None);
        assert_eq!(inventory.larger_scale_at([3., 3.], 50_000.), Some(22_000));
        // Inside the hole only the coarser coverage applies.
        assert_eq!(
            inventory.larger_scale_at([1.5, 0.5], 180_000.),
            Some(90_000)
        );
        assert_eq!(inventory.larger_scale_at([8., 8.], 90_000.), None);
        assert_eq!(inventory.larger_scale_at([20., 8.], 1e9), None);
    }
}
