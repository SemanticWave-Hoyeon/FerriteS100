//! Main wgpu Renderer
//!
//! Orchestrates rendering of drawing instructions to the screen.
//! Uses resvg for SVG symbol rendering via textures.

// Opt-in runtime diagnostics. A frame owns one collector; no diagnostic clocks or
// instruction guard allocations are created when the collector is absent.
type FlatDiagnosticCell =
    std::rc::Rc<std::cell::RefCell<ferrite_render::flat_reuse_diagnostics::FlatFrameSample>>;
struct FlatDiagnosticSpan {
    cell: FlatDiagnosticCell,
    stage: ferrite_render::flat_reuse_diagnostics::FlatFrameStage,
    start: std::time::Instant,
}
impl Drop for FlatDiagnosticSpan {
    fn drop(&mut self) {
        self.cell.borrow_mut().record_span(
            self.stage,
            self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        );
    }
}

// =============================================================================
// S-100/S-101 Symbol Scaling Constants
// =============================================================================
// Reference: S-100 Edition 5.0, Part 12 - Portrayal; IHO S-52 Presentation Library
//
// Symbol sizes in the Portrayal Catalogue (SVG viewBox) are defined in millimeters
// for vector quality. However, these are NOT the intended physical display sizes.
//
// S-100/S-52 specifies that symbols should be displayed at sizes that ensure:
// - Readability at normal viewing distance (约70cm for ECDIS)
// - Consistent appearance across different display densities
// - Symbols typically appear 2-5mm physical size on screen
//
// The 0.3mm/pixel reference in S-100 is for MINIMUM LEGIBLE FEATURE SIZE
// (line widths, text heights), not for symbol scaling.

/// Standard screen DPI (96 DPI = 3.78 pixels per mm)
/// This is the typical display density for computer monitors.
const SCREEN_PX_PER_MM: f32 = 96.0 / 25.4;

// Note: S-100 symbol sizing works as follows:
// 1. SVG symbols have mm dimensions (e.g., ACHBRT07 = 5.38mm wide)
// 2. usvg converts mm → user units (px at 96 DPI): 5.38mm → 20.3 user units
// 3. Texture is rendered at tree_size × render_scale (7.56) → ~154px
// 4. To display at correct mm size: display_scale = 1.0 / render_scale
//    (which recovers the original user-unit size = physical mm size at 96 DPI)
// The formula is: display_scale = instance.scale / tex.render_scale * self.symbol_scale

use ferrite_kernel::{CompositionPlane, CompositionStage};

// Aliases retain the exact ordered tuple payloads used by public diagnostics.
type PatternDrawRange = (
    CompositionPlane,
    i32,
    usize,
    usize,
    String,
    u8,
    Option<usize>,
);
pub type DisplayedSymbol = (
    String,
    Option<i64>,
    ferrite_render::WorldPoint,
    ferrite_render::ScreenPoint,
    i32,
    Option<u32>,
    CompositionPlane,
);
pub type DisplayedSymbolWithSource = (
    String,
    Option<i64>,
    ferrite_render::WorldPoint,
    ferrite_render::ScreenPoint,
    i32,
    Option<u32>,
    CompositionPlane,
    Option<usize>,
);
type ChartTextShape = (
    CompositionPlane,
    i32,
    Option<usize>,
    u8,
    egui::epaint::ClippedShape,
);

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::draw_range_index::{selected_indices, DrawKind, DrawRangeIndex};
use crate::egui_integration;
use crate::profiler::{CpuProfiler, GpuProfilerWrapper, ScopeTimer};
use ferrite_portrayal_catalog::ColorProfile;
use ferrite_render::{
    intern_symbol, Color, DrawingInstruction, RenderContext, ScreenPoint, SymbolId, WorldPoint,
};

use crate::egui_integration::{AppUiState, EguiIntegration, SettingsState};
use crate::pipeline::{PatternVertex, TextureVertex};
use crate::{
    GpuState, LineVertex, RenderPipelines, Result, SymbolCache, Vertex2D, ViewUniforms, WgpuError,
};

// === Symbol Classification Flags ===
// Cached per SymbolId to avoid repeated starts_with() string matching in hot paths.
// Computed once per unique symbol, reused across all frames.
const SYM_SOUNDING: u8 = 1 << 0; // starts_with("SOUND")
const SYM_NAV_AID: u8 = 1 << 1; // LIGHTS, BUOY, BCN, TOPMAR
const SYM_SAFETY: u8 = 1 << 2; // ISODGR, DANGER, WRECKS, OBSTRN, UWTROC, FOULAR

/// Classify a symbol string into bitflags (called once per unique symbol)
#[inline]
fn classify_symbol(s: &str) -> u8 {
    let mut flags = 0u8;
    if s.starts_with("SOUND") {
        flags |= SYM_SOUNDING;
    }
    if s.starts_with("LIGHTS")
        || s.starts_with("BUOY")
        || s.starts_with("BCN")
        || s.starts_with("TOPMAR")
    {
        flags |= SYM_NAV_AID;
    }
    if s == "ISODGR01"
        || s == "DANGER02"
        || s == "DANGER01"
        || s == "DANGER03"
        || s.starts_with("WRECKS")
        || s.starts_with("OBSTRN")
        || s.starts_with("UWTROC")
        || s.starts_with("FOULAR")
    {
        flags |= SYM_SAFETY;
    }
    flags
}

/// Ray-casting point-in-polygon test.
/// Returns true if point (px, py) is inside the given ring (list of (x,y) vertices).
fn point_in_ring(px: f32, py: f32, ring: &[(f32, f32)]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Cached triangulation result (world-coordinate earcut)
/// Stores indices and cleaned world vertices so earcut runs only once per polygon shape.
struct CachedTriangulation {
    /// Earcut triangle indices (into world_vertices)
    indices: Vec<usize>,
    /// Cleaned world-coordinate vertices [x0, y0, x1, y1, ...] (exterior + holes)
    world_vertices: Vec<f64>,
    /// World-coordinate axis-aligned bounding box (min_x, min_y, max_x, max_y)
    /// Used for O(1) viewport frustum culling — skip entire area if AABB is off-screen
    world_aabb: (f64, f64, f64, f64),
}

// Default OFF keeps the original by-value cache allocation behavior.
enum TriangulationStorage {
    Owned(CachedTriangulation),
    Shared(Arc<CachedTriangulation>),
}
impl std::ops::Deref for TriangulationStorage {
    type Target = CachedTriangulation;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(v) => v,
            Self::Shared(v) => v,
        }
    }
}
enum AreaRetainedResult {
    Ready(Arc<CachedTriangulation>),
    Rejected,
}
struct AreaRetainedRecord {
    ordinal: usize,
    projection: ferrite_render::FlatProjection,
    result: AreaRetainedResult,
}
#[derive(Default)]
struct AreaTriangulationRetention {
    epoch: Option<ferrite_render::StaticAreaGeometryEpoch>,
    records: Vec<AreaRetainedRecord>,
    payload_bytes: usize,
    retained_bytes: usize,
    rebind_hits: u64,
    rebind_rejections: u64,
    precomputed_areas: u64,
    cache_hits: u64,
    cold_evaluations: u64,
    peak_retained_bytes: usize,
}
impl AreaTriangulationRetention {
    const BYTE_CAP: usize = 32 * 1024 * 1024;
    const COUNT_CAP: usize = 4096;
    fn reset(&mut self) {
        self.records = Vec::new();
        self.epoch = None;
        self.payload_bytes = 0;
        self.retained_bytes = 0;
    }
    fn rebind(
        &mut self,
        context: &RenderContext,
        out: &mut HashMap<(usize, usize, ferrite_render::FlatProjection), TriangulationStorage>,
        rejected: &mut FxHashSet<(usize, usize, ferrite_render::FlatProjection)>,
    ) {
        if self.epoch != Some(context.static_area_geometry_epoch()) {
            return;
        }
        for record in &self.records {
            if record.projection != context.scaler.projection() {
                continue;
            }
            // Every pointer key comes from the fresh current owner, never from
            // the retained cache's previous allocation identity.
            let Some(ferrite_render::DrawingInstruction::Area(area)) =
                context.raw_instructions().get(record.ordinal)
            else {
                continue;
            };
            let key = WgpuRenderer::area_geometry_key(area, record.projection);
            match &record.result {
                AreaRetainedResult::Ready(v) => {
                    out.insert(key, TriangulationStorage::Shared(Arc::clone(v)));
                    self.rebind_hits = self.rebind_hits.saturating_add(1);
                }
                AreaRetainedResult::Rejected => {
                    rejected.insert(key);
                    self.rebind_rejections = self.rebind_rejections.saturating_add(1);
                }
            }
        }
    }

    fn admit(&mut self, record: AreaRetainedRecord, budget: usize, count: usize) -> bool {
        let payload = match &record.result {
            AreaRetainedResult::Rejected => Some(0),
            AreaRetainedResult::Ready(v) => v
                .world_vertices
                .capacity()
                .checked_mul(std::mem::size_of::<f64>())
                .and_then(|n| {
                    v.indices
                        .capacity()
                        .checked_mul(std::mem::size_of::<usize>())
                        .and_then(|m| n.checked_add(m))
                })
                .and_then(|n| {
                    n.checked_add(
                        std::mem::size_of::<CachedTriangulation>()
                            + 2 * std::mem::size_of::<usize>(),
                    )
                }),
        };
        let Some(payload) = payload.and_then(|n| n.checked_add(self.payload_bytes)) else {
            return false;
        };
        if self.records.len() >= count {
            return false;
        }
        let Some(minimum) = self
            .records
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_mul(std::mem::size_of::<AreaRetainedRecord>()))
            .and_then(|n| n.checked_add(payload))
        else {
            return false;
        };
        if minimum > budget {
            return false;
        }
        if self.records.len() == self.records.capacity() {
            let capacity = self.records.capacity().saturating_mul(2).max(16).min(count);
            if capacity <= self.records.len()
                || capacity
                    .checked_mul(std::mem::size_of::<AreaRetainedRecord>())
                    .and_then(|n| n.checked_add(payload))
                    .is_none_or(|n| n > budget)
            {
                return false;
            }
            if self
                .records
                .try_reserve_exact(capacity - self.records.len())
                .is_err()
            {
                return false;
            }
        }
        let Some(actual) = self
            .records
            .capacity()
            .checked_mul(std::mem::size_of::<AreaRetainedRecord>())
            .and_then(|n| n.checked_add(payload))
        else {
            return false;
        };
        if actual > budget {
            self.reset();
            return false;
        }
        self.records.push(record);
        self.payload_bytes = payload;
        self.retained_bytes = actual;
        self.peak_retained_bytes = self.peak_retained_bytes.max(actual);
        true
    }
}

#[cfg(test)]
mod area_retention_tests {
    use super::*;
    fn context() -> RenderContext {
        let mut c = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
        c.set_instructions_from_cache(vec![ferrite_render::DrawingInstruction::Area(
            ferrite_render::AreaInstruction::new(vec![
                WorldPoint::new(0., 0.),
                WorldPoint::new(1., 0.),
                WorldPoint::new(0., 1.),
            ]),
        )]);
        c.get_sorted_instructions();
        c
    }
    fn value() -> Arc<CachedTriangulation> {
        Arc::new(CachedTriangulation {
            indices: vec![2, 0, 1],
            world_vertices: vec![-0., 1., 2., 3., 4., 5.],
            world_aabb: (-0., 1., 4., 5.),
        })
    }
    fn record(result: AreaRetainedResult) -> AreaRetainedRecord {
        AreaRetainedRecord {
            ordinal: 0,
            projection: ferrite_render::FlatProjection::LocalGeographic,
            result,
        }
    }
    #[test]
    fn owner_rebinding_preserves_every_numeric_bit_and_index_without_old_pointer() {
        let old = context();
        let mut next = context();
        assert!(next.inherit_static_area_geometry_from(&old));
        let v = value();
        let mut r = AreaTriangulationRetention::default();
        r.epoch = Some(old.static_area_geometry_epoch());
        assert!(r.admit(record(AreaRetainedResult::Ready(Arc::clone(&v))), 4096, 16));
        let mut out = HashMap::new();
        let mut rejected = FxHashSet::default();
        r.rebind(&next, &mut out, &mut rejected);
        let ferrite_render::DrawingInstruction::Area(a) = &next.raw_instructions()[0] else {
            unreachable!()
        };
        let ferrite_render::DrawingInstruction::Area(b) = &old.raw_instructions()[0] else {
            unreachable!()
        };
        let key = WgpuRenderer::area_geometry_key(a, next.scaler.projection());
        assert_ne!(
            key,
            WgpuRenderer::area_geometry_key(b, old.scaler.projection())
        );
        let actual = &out[&key];
        assert_eq!(actual.indices, v.indices);
        assert_eq!(
            actual
                .world_vertices
                .iter()
                .map(|x| x.to_bits())
                .collect::<Vec<_>>(),
            v.world_vertices
                .iter()
                .map(|x| x.to_bits())
                .collect::<Vec<_>>()
        );
        assert_eq!(actual.world_aabb.0.to_bits(), v.world_aabb.0.to_bits());
        assert!(rejected.is_empty());
        assert_eq!(r.rebind_hits, 1);
    }
    #[test]
    fn rejection_projection_and_unadmitted_source_are_not_false_cache_hits() {
        let old = context();
        let mut next = context();
        assert!(next.inherit_static_area_geometry_from(&old));
        let mut r = AreaTriangulationRetention::default();
        r.epoch = Some(old.static_area_geometry_epoch());
        assert!(r.admit(record(AreaRetainedResult::Rejected), 4096, 16));
        let mut out = HashMap::new();
        let mut rejected = FxHashSet::default();
        r.rebind(&next, &mut out, &mut rejected);
        assert_eq!(rejected.len(), 1);
        assert!(out.is_empty());
        rejected.clear();
        next.scaler
            .set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
        r.rebind(&next, &mut out, &mut rejected);
        assert!(rejected.is_empty());
        let fresh = context();
        r.rebind(&fresh, &mut out, &mut rejected);
        assert!(out.is_empty());
        assert!(rejected.is_empty());
    }
    #[test]
    fn payload_capacity_count_caps_and_decline_do_not_remove_original_results() {
        let mut r = AreaTriangulationRetention::default();
        let v = value();
        assert!(!r.admit(record(AreaRetainedResult::Ready(Arc::clone(&v))), 1, 16));
        assert!(r.records.is_empty());
        assert!(r.admit(record(AreaRetainedResult::Ready(Arc::clone(&v))), 4096, 1));
        assert!(r.retained_bytes <= 4096);
        assert!(!r.admit(record(AreaRetainedResult::Rejected), 4096, 1));
        assert_eq!(r.records.len(), 1);
        assert_eq!(v.indices, [2, 0, 1]);
        r.reset();
        assert_eq!(r.retained_bytes, 0);
        assert!(r.records.is_empty());
        assert!(r.epoch.is_none());
    }
}

/// Batched symbols grouped by texture for efficient rendering
#[allow(dead_code)]
struct SymbolBatch {
    /// All vertices for this texture batch
    vertices: Vec<TextureVertex>,
    /// All indices for this texture batch
    indices: Vec<u32>,
    /// The texture bind group
    bind_group_idx: usize,
}

/// Simple Quadtree node for spatial indexing
#[allow(dead_code)]
struct QuadTreeNode {
    bounds: (f64, f64, f64, f64), // (min_x, min_y, max_x, max_y)
    feature_ids: Vec<i64>,
    children: Option<Box<[QuadTreeNode; 4]>>,
}

#[allow(dead_code)]
impl QuadTreeNode {
    fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        QuadTreeNode {
            bounds: (min_x, min_y, max_x, max_y),
            feature_ids: Vec::new(),
            children: None,
        }
    }

    /// Query features intersecting with viewport
    fn query(&self, viewport: (f64, f64, f64, f64), result: &mut Vec<i64>) {
        // Check if this node intersects viewport
        if !Self::intersects(self.bounds, viewport) {
            return;
        }

        // Add features from this node
        result.extend(&self.feature_ids);

        // Recurse into children
        if let Some(ref children) = self.children {
            for child in children.iter() {
                child.query(viewport, result);
            }
        }
    }

    #[inline]
    fn intersects(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
        a.0 <= b.2 && a.2 >= b.0 && a.1 <= b.3 && a.3 >= b.1
    }
}

/// Hash a slice of world points for cache key
#[allow(dead_code)]
fn hash_geometry(points: &[WorldPoint]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for p in points {
        ((p.x * 1_000_000.0) as i64).hash(&mut hasher);
        ((p.y * 1_000_000.0) as i64).hash(&mut hasher);
    }
    hasher.finish()
}

/// Cached GPU texture for a pattern fill (uses Repeat sampler)
struct PatternTexture {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    /// Texture dimensions in pixels (matches tiling period exactly)
    width: u32,
    height: u32,
}

/// Cached GPU texture for a symbol
pub(crate) struct SymbolTexture {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    pub(crate) bind_group: wgpu::BindGroup,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Pivot position within texture (in pixels from top-left at render_scale)
    pub(crate) pivot_in_texture: (f32, f32),
    pub(crate) render_scale: f32,
    pub(crate) has_coverage: bool,
}

/// Symbol instance to render
/// Memory optimized: uses interned SymbolId (4 bytes) instead of String (24 bytes)
#[derive(Clone, Copy)]
struct SymbolInstance {
    source: Option<usize>,
    plane: CompositionPlane,
    feature_id: Option<i64>,
    cell_index: Option<u32>,
    world: ferrite_render::WorldPoint,
    priority: i32,
    anchor: [f32; 2],
    symbol_id: ferrite_render::SymbolId,
    screen_x: f32,
    screen_y: f32,
    scale: f32,
    rotation: f32,
}

/// Pending text label to render via egui painter overlay
struct TextLabel {
    source: Option<usize>,
    plane: CompositionPlane,
    priority: i32,
    anchor: [f32; 2],
    rotation: f32,
    screen_x: f32,
    screen_y: f32,
    text: String,
    font_size: f32,
    color: [f32; 4],
    background: Option<[f32; 4]>,
    bold: bool,
    italic: bool,
    h_align: ferrite_render::HAlign,
    v_align: ferrite_render::VAlign,
}

/// wgpu-based chart renderer
#[derive(Clone, Copy)]
enum CoveragePrimitive {
    Area,
    Line,
    Symbol,
    Pattern,
    Text,
}
struct GpuChartText {
    source: Option<usize>,
    wrap_pass: u8,
    plane: CompositionPlane,
    priority: i32,
    scissor: [u32; 4],
    bind_group: wgpu::BindGroup,
    vertices: Arc<wgpu::Buffer>,
    indices: Arc<wgpu::Buffer>,
    index_count: u32,
}
/// Retain a bounded set of ordered glyph buffers instead of allocating every frame.
/// Each active draw retains an Arc; queue writes and submissions share one GPU queue.
#[derive(Default)]
struct ChartTextBufferPool {
    slots: Vec<(Arc<wgpu::Buffer>, Arc<wgpu::Buffer>)>,
    cursor: usize,
    retained_bytes: u64,
    allocations: u64,
    writes: u64,
}
impl ChartTextBufferPool {
    const BUDGET: u64 = 16 * 1024 * 1024;
    fn begin(&mut self) {
        self.cursor = 0;
    }
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        vertices: &[crate::ChartTextVertex],
        indices: &[u32],
    ) -> (Arc<wgpu::Buffer>, Arc<wgpu::Buffer>) {
        let vb = bytemuck::cast_slice(vertices);
        let ib = bytemuck::cast_slice(indices);
        let sizes = [
            (vb.len() as u64).max(4).next_power_of_two(),
            (ib.len() as u64).max(4).next_power_of_two(),
        ];
        let fits = self
            .slots
            .get(self.cursor)
            .is_some_and(|(v, i)| v.size() >= vb.len() as u64 && i.size() >= ib.len() as u64);
        let pair = if fits {
            self.slots[self.cursor].clone()
        } else {
            let make = |size, usage, label| {
                Arc::new(device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size,
                    usage: usage | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }))
            };
            let pair = (
                make(sizes[0], wgpu::BufferUsages::VERTEX, "pooled-chart-glyphs"),
                make(
                    sizes[1],
                    wgpu::BufferUsages::INDEX,
                    "pooled-chart-glyph-indices",
                ),
            );
            self.allocations += 2;
            let previous = self
                .slots
                .get(self.cursor)
                .map_or(0, |(v, i)| v.size() + i.size());
            let total = self.retained_bytes - previous + sizes[0] + sizes[1];
            if total <= Self::BUDGET {
                if self.cursor < self.slots.len() {
                    self.slots[self.cursor] = pair.clone();
                } else {
                    self.slots.push(pair.clone());
                }
                self.retained_bytes = total;
            }
            pair
        };
        queue.write_buffer(&pair.0, 0, vb);
        queue.write_buffer(&pair.1, 0, ib);
        self.writes += 2;
        self.cursor += 1;
        pair
    }
}
#[derive(Clone)]
struct GpuRasterLayer {
    continuous_identity: Option<crate::continuous_frame_binding::PreparedContinuousIdentity>,
    index_count: u32,
    continuous: Option<ferrite_render::ContinuousRasterMetadata>,
    viewing_groups: Vec<u32>,
    draw_order: ferrite_render::RasterDrawOrder,
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    bounds: ferrite_render::GeoBounds,
    grid: ferrite_render::RasterGrid,
    tile_size: [u32; 2],
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
}

/// Prepared GPU resources, not an installed scene. Dropping after any producer
/// failure releases all textures. A qualified promotion token is required later.
pub struct PreparedRasterMaterialBatch {
    layers: Vec<GpuRasterLayer>,
    owner: std::sync::Arc<()>,
}

/// Complete unpublished raster inventory. Continuous proof headers are written
/// only into owned textures; retained live textures are never modified by staging.
pub struct PreparedRasterPublication {
    layers: Vec<GpuRasterLayer>,
    proof: Option<crate::ValidatedContinuousFrame>,
    owner: Arc<()>,
    previous_epoch: Arc<()>,
    frame: crate::continuous_frame_binding::ContinuousFrameBinding,
    transform: [u32; 9],
}

/// Raster materials form one visible atomic commit.
pub struct PreparedRasterScenePublication {
    raster: PreparedRasterPublication,
}

/// Backend execution evidence for S-100 Parent dependencies.
#[derive(Debug, Clone, Default)]
pub struct DependencyRenderStatus {
    pub iterations: usize,
    pub converged: bool,
    pub missing_parent_count: usize,
    pub nonconvergent_diagnostic: bool,
    pub permitted: Vec<bool>,
    pub executed: Vec<bool>,
}

impl DependencyRenderStatus {
    pub fn audit_value(&self) -> serde_json::Value {
        serde_json::json!({"iterations":self.iterations,"converged":self.converged,
            "missing_parent_count":self.missing_parent_count,"nonconvergent_diagnostic":self.nonconvergent_diagnostic,
            "permitted":self.permitted,"executed":self.executed})
    }
}

fn create_symbol_texture(
    gpu: &GpuState,
    pipelines: &RenderPipelines,
    name: &str,
    geom: &crate::SymbolGeometry,
) -> SymbolTexture {
    let (texture, view) = gpu.create_texture_from_rgba(
        &geom.pixels,
        geom.width,
        geom.height,
        &format!("symbol_{name}"),
    );
    SymbolTexture {
        texture,
        bind_group: pipelines.create_texture_bind_group(&gpu.device, &view),
        width: geom.width,
        height: geom.height,
        pivot_in_texture: geom.pivot_in_texture(),
        render_scale: geom.render_scale,
        has_coverage: geom.pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0),
    }
}

fn raster_publication_identity_matches(
    owner: &Arc<()>,
    expected_owner: &Arc<()>,
    epoch: &Arc<()>,
    expected_epoch: &Arc<()>,
) -> bool {
    Arc::ptr_eq(owner, expected_owner) && Arc::ptr_eq(epoch, expected_epoch)
}

/// Optional hidden-test ownership evidence; independent of index min/max.
#[derive(Clone, Debug)]
struct PatternEmissionAudit {
    source_ordinal: usize,
    vertex_start: usize,
    vertex_end: usize,
    index_start: usize,
    index_end: usize,
    wrap_mode: u8,
    /// Actual CPU clipping translation in physical screen pixels, exact binary64.
    wrap_dx_screen_bits: u64,
}
const MAX_PATTERN_AUDIT_EMISSIONS: usize = 65536;
fn record_pattern_emission(
    records: &mut Vec<PatternEmissionAudit>,
    dropped: &mut usize,
    record: PatternEmissionAudit,
) {
    if records.len() < MAX_PATTERN_AUDIT_EMISSIONS {
        records.push(record);
    } else {
        *dropped = dropped.saturating_add(1);
    }
}

pub struct WgpuRenderer {
    draw_range_index_enabled: bool,

    spatial_hierarchy_enabled: bool,

    geometry_transform: Option<ferrite_render::FlatTransform>,
    raster_layers: Vec<GpuRasterLayer>,
    continuous_owner: std::sync::Arc<()>,
    raster_epoch: Arc<()>,
    continuous_uniforms: [Option<ViewUniforms>; 3],
    continuous_frame: Option<crate::ValidatedContinuousFrame>,
    // Updated only on scene commit: regular frames avoid scanning raster materials.
    continuous_layer_count: usize,
    raster_enabled_groups: Option<std::collections::HashSet<u32>>,
    pub state: GpuState,
    pub pipelines: RenderPipelines,
    pub view_buffer: wgpu::Buffer,
    pub view_bind_group: wgpu::BindGroup,
    /// Collected area vertices
    area_vertices: Vec<Vertex2D>,
    area_indices: Vec<u32>,
    /// Collected line vertices
    line_vertices: Vec<LineVertex>,
    temporal_visibility_counts: (usize, usize),
    temporal_visibility_mask: Vec<bool>,
    line_indices: Vec<u32>,
    /// GPU texture cache for symbols (keyed by interned SymbolId for cache efficiency)
    symbol_textures: HashMap<ferrite_render::SymbolId, SymbolTexture>,
    /// Symbol instances to render
    symbol_instances: Vec<SymbolInstance>,
    /// Background color
    pub background_color: Color,
    /// User-adjustable symbol scale factor (default 1.0)
    /// This is applied ON TOP of the S-100 standard sizing.
    /// 1.0 = standard S-100 size, 0.5 = half size, 2.0 = double size
    pub symbol_scale: f32,
    /// Show sounding symbols (viewing group 33010)
    pub show_soundings: bool,
    /// Current zoom level (1.0 = default, higher = zoomed in)
    pub zoom_level: f64,
    display_scale: u32,
    /// Chart compilation scale (e.g., 22000 for 1:22000)
    /// Used to calculate viewing scale for S-101 feature filtering
    pub compilation_scale: u32,
    /// Grid for symbol decluttering (screen space) - for non-sounding symbols
    symbol_grid: FxHashSet<(i32, i32)>,
    /// Screen-space grid for sounding decluttering
    sounding_screen_grid: FxHashMap<(i32, i32), ((i64, i64), f64)>,
    /// Exact positions of soundings that have been allowed through (world coordinates)
    sounding_exact_positions: FxHashSet<(i64, i64)>,
    /// World-coordinate deduplication (to remove exact duplicates from multiple charts)
    world_dedup: FxHashSet<(i64, i64, u64)>,
    /// Cached symbol classification flags (computed once per unique SymbolId, never cleared)
    symbol_class_cache: FxHashMap<SymbolId, u8>,
    /// Set of SymbolIds that could not be rendered (missing SVG, color profile, etc).
    /// Used to log each missing symbol exactly once instead of every frame, and to
    /// surface a "N symbols missing" count to the debug HUD/logs.
    missing_symbol_ids: FxHashSet<SymbolId>,
    /// Count of point instructions that arrived with an empty symbol_ref. Indicates a
    /// portrayal-rules bug (Lua emitted a Point without a symbol). Tracked but not
    /// rendered — silently swallowing this would hide chart-data quality issues.
    empty_symbol_ref_count: u32,
    /// Grid cell size in pixels (adjusted by zoom)
    grid_cell_size: f32,
    /// Sounding grid cell size in pixels (screen-space)
    /// Fixed size for consistent density regardless of zoom
    sounding_cell_size_px: f32,
    /// Skip screen-space decluttering during animation (when preserve_declutter is true)
    skip_screen_declutter: bool,
    /// Screen-space pan offset (pixels) for fast panning during drag
    screen_pan_offset: (f32, f32),
    selection_anchor: Option<[f32; 2]>,
    selection_world_geometry: Vec<Vec<WorldPoint>>,
    selection_screen_geometry: Vec<Vec<[f32; 2]>>,
    displayed_geometry: Vec<usize>,
    selection_index: std::cell::OnceCell<ferrite_render::SelectionIndex>,
    empty_selection_stats: ferrite_render::SelectionIndexStats,
    dependency_status: DependencyRenderStatus,
    /// GPU zoom scale for smooth zooming (1.0 = no zoom delta, rebuilt at this level)
    screen_zoom_scale: f32,
    screen_zoom_scale_y: f32,
    /// Zoom pivot point in screen coordinates
    screen_zoom_pivot: (f32, f32),
    /// egui integration for UI overlay
    egui: EguiIntegration,
    /// UI state shared with main app
    pub ui_state: AppUiState,
    // === OPTIMIZATION FIELDS ===
    /// Geometry allocations are unique only within an unchanged instruction lifetime.
    triangulation_cache:
        HashMap<(usize, usize, ferrite_render::FlatProjection), TriangulationStorage>,
    triangulation_revision: Option<u64>,
    area_triangulation_reuse_enabled: bool,
    retained_area_triangulations: AreaTriangulationRetention,
    triangulation_failures: FxHashSet<(usize, usize, ferrite_render::FlatProjection)>,
    /// Batched symbols by texture (optimization, keyed by interned SymbolId)
    /// Packed symbol vertices for single-buffer rendering
    packed_symbol_vertices: Vec<TextureVertex>,
    /// Packed symbol indices for single-buffer rendering
    packed_symbol_indices: Vec<u32>,
    /// Ranges into packed arrays per symbol texture: (symbol_id, index_start, index_count)
    packed_symbol_ranges: Vec<(SymbolId, u32, u32)>,
    /// Animation/drag mode - enables fast-path rendering
    pub animation_mode: bool,
    view_dependent_symbols: bool,
    view_clipped_patterns: bool,
    flat_diagnostic: Option<FlatDiagnosticCell>,
    flat_gpu_coverage_host_ns: u64,
    /// LOD level (0=full detail, 1=medium, 2=low)
    /// Viewport bounds in world coordinates for culling
    viewport_world_bounds: Option<(f64, f64, f64, f64)>,
    prepared_coverage: Option<Arc<ferrite_render::PreparedCoverage>>,
    coverage_frame: Option<crate::coverage_gpu_frame::CoverageGpuFrame>,
    coverage_pipelines: Option<crate::coverage_pipeline::CoveragePipelines>,
    frame_local_coverage_clip_reuse: bool,
    coverage_failed: bool,
    emitting_coverage_source: Option<usize>,
    device_fixed_sources: FxHashSet<usize>,
    // === S-101 PRIORITY GROUP RENDERING ===
    /// Area index ranges by priority: (display_plane, priority, start_index, end_index)
    area_priority_ranges: Vec<(CompositionPlane, i32, usize, usize, Option<usize>)>,
    /// Line index ranges by priority: (display_plane, priority, start_index, end_index)
    line_priority_ranges: Vec<(CompositionPlane, i32, usize, usize, Option<usize>)>,
    /// Symbol instance ranges by priority: (display_plane, priority, start_index, end_index)
    symbol_priority_ranges: Vec<(CompositionPlane, i32, usize, usize, Option<usize>)>,
    // === PATTERN FILL (S-100 GPU texture-repeat tiling) ===
    /// Pattern fill vertices (TextureVertex: position + inv_tile_size)
    pattern_vertices: Vec<PatternVertex>,
    pattern_emission_audit: Option<Vec<PatternEmissionAudit>>,
    pattern_emission_audit_dropped: usize,
    /// Pattern fill indices
    pattern_indices: Vec<u32>,
    /// Pattern fill ranges: (display_plane, priority, index_start, index_end, pattern_texture_key)
    pattern_ranges: Vec<PatternDrawRange>,
    /// Pattern fill GPU textures (keyed by "{symbol}_pat")
    pattern_textures: HashMap<String, PatternTexture>,
    /// Pending text labels to render via egui painter
    text_labels: Vec<TextLabel>,
    chart_text_shapes: Vec<(
        CompositionPlane,
        i32,
        Option<usize>,
        u8,
        egui::epaint::ClippedShape,
    )>,
    chart_text_meshes: Vec<GpuChartText>,
    chart_text_buffers: ChartTextBufferPool,
    /// Grid for text collision avoidance
    // === CACHED GPU BUFFERS (avoid recreating every frame) ===
    /// Cached area vertex buffer (rebuilt only when geometry changes)
    cached_area_vb: Option<wgpu::Buffer>,
    cached_area_ib: Option<wgpu::Buffer>,
    cached_area_index_count: u32,
    /// Cached line vertex buffer
    cached_line_vb: Option<wgpu::Buffer>,
    cached_line_ib: Option<wgpu::Buffer>,
    cached_line_index_count: u32,
    /// Cached pattern vertex buffer
    cached_pattern_vb: Option<wgpu::Buffer>,
    cached_pattern_ib: Option<wgpu::Buffer>,
    cached_pattern_index_count: u32,
    /// Whether cached GPU buffers are stale and need rebuild
    gpu_buffers_dirty: bool,
    /// Immutable INDEX-only quad topology, shared by all instanced symbol batches.
    /// Never rewritten, including during candidate publication.
    shared_symbol_quad_index_buffer: Option<wgpu::Buffer>,
    immutable_instance_cache:
        Option<crate::immutable_payload_cache::ImmutablePayloadCache<wgpu::Buffer>>,
    /// Cached symbol GPU buffers per priority range (avoid recreating every frame)
    #[allow(clippy::type_complexity)]
    cached_symbol_buffers: Vec<(
        CompositionPlane,
        i32,
        usize,
        usize,
        wgpu::Buffer,
        wgpu::Buffer,
        Vec<(SymbolId, u32, u32)>,
    )>,
    // === INSTRUCTION CACHE (reused across rebuilds when only view changes) ===
    /// Content- and visibility-aware shared suppression plan.
    line_suppression: ferrite_render::LineSuppressionCache,
    // === PROFILING ===
    /// CPU-side performance profiler
    pub cpu_profiler: CpuProfiler,
    /// GPU-side profiler (wgpu-profiler)
    gpu_profiler: GpuProfilerWrapper,
    // === BACKGROUND WORLD MAP (Natural Earth) ===
    /// Pre-parsed coastline segments from Natural Earth 110m GeoJSON
    /// Each inner Vec is a line string: list of [longitude, latitude] pairs
    world_map_coastlines: ferrite_render::BackgroundCoastlines,
    world_map_detailed: ferrite_render::BackgroundCoastlines,
    /// Geometry viewport shared by native and exported chart passes.
    chart_geometry_viewport: Option<ferrite_render::Viewport>,
    /// Bounding boxes of loaded chart cells (world coords).
    /// Used to draw opaque background rectangles that mask world map under charts.
    world_map_chart_boxes: Vec<(f64, f64, f64, f64)>,
    /// World map line vertices (separate from chart line_vertices)
    world_map_line_vertices: Vec<LineVertex>,
    world_map_line_indices: Vec<u32>,
    /// Opaque background rectangles over chart bboxes (mask world map under charts)
    world_map_mask_vertices: Vec<Vertex2D>,
    world_map_mask_indices: Vec<u32>,
    /// Cached GPU buffers for world map
    cached_wm_line_vb: Option<wgpu::Buffer>,
    cached_wm_line_ib: Option<wgpu::Buffer>,
    cached_wm_mask_vb: Option<wgpu::Buffer>,
    cached_wm_mask_ib: Option<wgpu::Buffer>,
    // === LONGITUDE WRAPPING (infinite horizontal panning) ===
    /// Screen pixels corresponding to 360° of longitude (0 = wrapping disabled)
    lon_wrap_screen_px: f32,
    /// View uniform buffer for left (-360°) wrapping copy
    view_buffer_left: wgpu::Buffer,
    /// View uniform buffer for right (+360°) wrapping copy
    view_buffer_right: wgpu::Buffer,
    /// Bind group for left wrapping view
    view_bind_group_left: wgpu::BindGroup,
    /// Bind group for right wrapping view
    view_bind_group_right: wgpu::BindGroup,
}

impl GpuRasterLayer {
    fn admit(
        &self,
        budget: &mut crate::raster_publication_budget::RasterPublicationBudget,
    ) -> Result<()> {
        let texture = u64::from(self._texture.width()) * u64::from(self._texture.height()) * 4;
        let geometry = self
            .vertices
            .size()
            .checked_add(self.indices.size())
            .ok_or_else(|| WgpuError::Render("Raster geometry size overflow".into()))?;
        let identity = self
            .continuous_identity
            .as_ref()
            .map_or(0, |i| (i.vertex_bytes.len() + i.index_bytes.len()) as u64);
        budget.admit(texture, geometry, identity)
    }
}

impl WgpuRenderer {
    /// Create new renderer for window
    pub async fn new(window: Arc<Window>) -> Result<Self> {
        let state = GpuState::new(window.clone()).await?;
        let pipelines = RenderPipelines::new(&state)?;
        let gpu_profiler = GpuProfilerWrapper::new(&state.device);

        let (width, height) = state.viewport_size();
        let uniforms = ViewUniforms::new(width, height, 1.0);
        let view_buffer = state.create_uniform_buffer(&uniforms, "view_uniforms");
        let view_bind_group = pipelines.create_view_bind_group(&state.device, &view_buffer);

        // Create wrapping view buffers for ±360° longitude copies
        let view_buffer_left = state.create_uniform_buffer(&uniforms, "view_uniforms_left");
        let view_bind_group_left =
            pipelines.create_view_bind_group(&state.device, &view_buffer_left);
        let view_buffer_right = state.create_uniform_buffer(&uniforms, "view_uniforms_right");
        let view_bind_group_right =
            pipelines.create_view_bind_group(&state.device, &view_buffer_right);

        // Create egui integration (render without MSAA for crisp text)
        let egui = EguiIntegration::new(
            &state.device,
            state.format(),
            1, // No MSAA for egui
            window,
        );

        Ok(WgpuRenderer {
            draw_range_index_enabled: std::env::var("FERRITE_DRAW_RANGE_INDEX").as_deref()
                == Ok("1"),
            state,
            pipelines,
            view_buffer,
            view_bind_group,
            // Pre-allocate with typical initial capacities to avoid reallocation
            area_vertices: Vec::with_capacity(10000),
            area_indices: Vec::with_capacity(30000),
            line_vertices: Vec::with_capacity(5000),
            temporal_visibility_counts: (0, 0),
            temporal_visibility_mask: Vec::new(),
            line_indices: Vec::with_capacity(15000),
            raster_layers: Vec::new(),
            continuous_owner: std::sync::Arc::new(()),
            raster_epoch: Arc::new(()),
            continuous_uniforms: [Some(uniforms), None, None],
            continuous_frame: None,
            continuous_layer_count: 0,
            raster_enabled_groups: None,

            symbol_textures: HashMap::with_capacity(100),
            symbol_instances: Vec::with_capacity(2000),
            background_color: Color::from_u8(201, 237, 255, 255), // DEPDW (deep water) — matches S-101 default
            symbol_scale: 1.0, // S-100 standard: 1.0 = nominal symbol size at 0.3mm/pixel
            show_soundings: true, // Visibility controlled by S-101 viewing groups
            zoom_level: 1.0,
            display_scale: 1,
            compilation_scale: 22000, // Default compilation scale (1:22000)
            symbol_grid: FxHashSet::with_capacity_and_hasher(1000, Default::default()),
            sounding_screen_grid: FxHashMap::with_capacity_and_hasher(2000, Default::default()),
            sounding_exact_positions: FxHashSet::with_capacity_and_hasher(5000, Default::default()),
            world_dedup: FxHashSet::with_capacity_and_hasher(5000, Default::default()),
            symbol_class_cache: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            missing_symbol_ids: FxHashSet::with_capacity_and_hasher(32, Default::default()),
            empty_symbol_ref_count: 0,

            grid_cell_size: 30.0,         // Default grid cell size in pixels
            sounding_cell_size_px: 150.0, // Fixed pixel spacing between soundings
            skip_screen_declutter: false,
            screen_pan_offset: (0.0, 0.0),
            selection_anchor: None,
            selection_world_geometry: Vec::new(),
            selection_screen_geometry: Vec::new(),
            displayed_geometry: Vec::new(),
            selection_index: std::cell::OnceCell::new(),
            empty_selection_stats: ferrite_render::SelectionIndexStats::default(),
            dependency_status: DependencyRenderStatus::default(),
            screen_zoom_scale: 1.0,
            screen_zoom_scale_y: 1.0,
            screen_zoom_pivot: (0.0, 0.0),
            egui,
            ui_state: AppUiState::default(),
            // Optimization fields
            triangulation_cache: HashMap::with_capacity(500),
            triangulation_failures: FxHashSet::default(),
            triangulation_revision: None,
            area_triangulation_reuse_enabled: ferrite_render::area_triangulation_reuse_enabled(),
            retained_area_triangulations: AreaTriangulationRetention::default(),
            packed_symbol_vertices: Vec::with_capacity(4000),
            packed_symbol_indices: Vec::with_capacity(6000),
            packed_symbol_ranges: Vec::with_capacity(50),
            animation_mode: false,
            view_dependent_symbols: false,
            view_clipped_patterns: false,
            flat_diagnostic: None,
            flat_gpu_coverage_host_ns: 0,
            viewport_world_bounds: None,
            prepared_coverage: None,
            coverage_frame: None,
            coverage_pipelines: None,
            frame_local_coverage_clip_reuse:
                crate::coverage_gpu_frame::frame_local_clip_reuse_policy(
                    std::env::var_os("FERRITE_FLAT_COVERAGE_CLIP_CSE").as_deref(),
                ),
            coverage_failed: false,
            emitting_coverage_source: None,
            device_fixed_sources: FxHashSet::default(),
            // S-101 priority group rendering
            area_priority_ranges: Vec::with_capacity(10),
            line_priority_ranges: Vec::with_capacity(10),
            symbol_priority_ranges: Vec::with_capacity(10),
            // Pattern fill
            pattern_vertices: Vec::with_capacity(5000),
            pattern_emission_audit: (crate::background_test::enabled()
                && std::env::var("FERRITE_PATTERN_EMISSION_AUDIT").as_deref() == Ok("1"))
            .then(Vec::new),
            pattern_emission_audit_dropped: 0,
            pattern_indices: Vec::with_capacity(15000),
            pattern_ranges: Vec::with_capacity(10),
            pattern_textures: HashMap::new(),
            text_labels: Vec::with_capacity(500),
            chart_text_shapes: Vec::new(),
            chart_text_meshes: Vec::new(),
            chart_text_buffers: ChartTextBufferPool::default(),
            // GPU buffer cache
            cached_area_vb: None,
            cached_area_ib: None,
            cached_area_index_count: 0,
            cached_line_vb: None,
            cached_line_ib: None,
            cached_line_index_count: 0,
            cached_pattern_vb: None,
            cached_pattern_ib: None,
            cached_pattern_index_count: 0,
            gpu_buffers_dirty: true,
            geometry_transform: None,
            cached_symbol_buffers: Vec::new(),
            shared_symbol_quad_index_buffer: None,
            immutable_instance_cache: (std::env::var("FERRITE_IMMUTABLE_INSTANCE_CACHE")
                .as_deref()
                == Ok("1"))
            .then(crate::immutable_payload_cache::ImmutablePayloadCache::new),
            line_suppression: ferrite_render::LineSuppressionCache::default(),
            // Profiling
            cpu_profiler: CpuProfiler::new(),
            gpu_profiler,
            // Background world map (empty until set_world_map is called)
            world_map_coastlines: Default::default(),
            world_map_detailed: Default::default(),
            chart_geometry_viewport: None,

            // Keep opt-in until representative frame-time gates pass.
            spatial_hierarchy_enabled: false,

            world_map_chart_boxes: Vec::new(),
            world_map_line_vertices: Vec::with_capacity(2000),
            world_map_line_indices: Vec::with_capacity(6000),
            world_map_mask_vertices: Vec::new(),
            world_map_mask_indices: Vec::new(),
            cached_wm_line_vb: None,
            cached_wm_line_ib: None,
            cached_wm_mask_vb: None,
            cached_wm_mask_ib: None,
            // Longitude wrapping
            lon_wrap_screen_px: 0.0,
            view_buffer_left,
            view_buffer_right,
            view_bind_group_left,
            view_bind_group_right,
        })
    }

    /// Enable or disable profiling (both CPU and GPU)
    pub fn set_profiling_enabled(&mut self, enabled: bool) {
        crate::profiler::set_profiling_enabled(enabled);
        self.gpu_profiler.set_enabled(enabled);
    }

    /// Flush profiler reports (call on shutdown)
    pub fn flush_profiler(&mut self) {
        self.cpu_profiler.flush();
    }

    /// Set animation mode for fast-path rendering during drag/zoom
    #[inline]
    pub fn set_animation_mode(&mut self, animating: bool) {
        self.animation_mode = animating;
    }

    /// CPU chart geometry must have been built with the currently requested view.
    pub fn geometry_matches_view(&self, scaler: &ferrite_render::Scaler) -> bool {
        self.geometry_transform == Some(Self::scaler_transform(scaler))
    }

    /// Update viewport world bounds for frustum culling
    pub fn update_viewport_bounds(&mut self, scaler: &ferrite_render::Scaler) {
        let (vw, vh) = (scaler.viewport.width, scaler.viewport.height);
        let top_left = scaler.screen_to_world(ScreenPoint { x: 0.0, y: 0.0 });
        let bottom_right = scaler.screen_to_world(ScreenPoint { x: vw, y: vh });
        self.viewport_world_bounds = Some((
            top_left.x.min(bottom_right.x),
            top_left.y.min(bottom_right.y),
            top_left.x.max(bottom_right.x),
            top_left.y.max(bottom_right.y),
        ));
    }

    /// Culling uses the same three longitude copies as the GPU geometry pass.
    #[inline]
    fn is_point_visible(&self, x: f64, y: f64) -> bool {
        longitude_bounds_visible(
            (x, y, x, y),
            self.viewport_world_bounds,
            self.lon_wrap_screen_px > 0.0,
        )
    }
    #[inline]
    fn is_aabb_visible(&self, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        longitude_bounds_visible(
            (ax, ay, bx, by),
            self.viewport_world_bounds,
            self.lon_wrap_screen_px > 0.0,
        )
    }
    fn is_ring_visible_static(
        ring: &[WorldPoint],
        bounds: Option<(f64, f64, f64, f64)>,
        wrapping: bool,
    ) -> bool {
        let mut aabb = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for p in ring {
            aabb.0 = aabb.0.min(p.x);
            aabb.1 = aabb.1.min(p.y);
            aabb.2 = aabb.2.max(p.x);
            aabb.3 = aabb.3.max(p.y);
        }
        longitude_bounds_visible(aabb, bounds, wrapping)
    }

    /// Clear triangulation cache (call when chart data changes)
    /// Distinct rejected area/projection pairs in the current geometry revision.
    pub fn rejected_area_fill_count(&self) -> usize {
        self.triangulation_failures.len()
    }
    pub fn clear_triangulation_cache(&mut self) {
        self.triangulation_cache.clear();
        self.triangulation_failures.clear();
        self.triangulation_revision = None;
        self.retained_area_triangulations.reset();
        self.line_suppression.clear();
    }

    fn bind_triangulation_context(&mut self, context: &RenderContext) {
        let revision = context.geometry_revision();
        if self.triangulation_revision != Some(revision) {
            self.triangulation_cache.clear();
            self.triangulation_failures.clear();
            self.triangulation_revision = Some(revision);
            if self.area_triangulation_reuse_enabled {
                if self.retained_area_triangulations.epoch
                    == Some(context.static_area_geometry_epoch())
                {
                    self.retained_area_triangulations.rebind(
                        context,
                        &mut self.triangulation_cache,
                        &mut self.triangulation_failures,
                    );
                } else {
                    self.retained_area_triangulations.reset();
                }
            }
        }
    }

    /// Pre-compute triangulations for all area instructions.
    /// Call after chart load to avoid cold-path stalls during first render frame.
    pub fn precompute_triangulations(&mut self, context: &RenderContext) {
        self.bind_triangulation_context(context);
        let mut count = 0;
        for instr in context.raw_instructions() {
            if let ferrite_render::DrawingInstruction::Area(area) = instr {
                if self.area_triangulation_reuse_enabled {
                    self.retained_area_triangulations.precomputed_areas = self
                        .retained_area_triangulations
                        .precomputed_areas
                        .saturating_add(1);
                }
                if self
                    .ensure_triangulated(area, context.scaler.projection())
                    .is_some()
                {
                    count += 1;
                }
            }
        }
        if self.area_triangulation_reuse_enabled {
            // Drop old retention before recapture; no two retained generations.
            self.retained_area_triangulations.reset();
            let projection = context.scaler.projection();
            for (ordinal, instruction) in context.raw_instructions().iter().enumerate() {
                let ferrite_render::DrawingInstruction::Area(area) = instruction else {
                    continue;
                };
                let key = Self::area_geometry_key(area, projection);
                let result = if let Some(TriangulationStorage::Shared(v)) =
                    self.triangulation_cache.get(&key)
                {
                    Some(AreaRetainedResult::Ready(Arc::clone(v)))
                } else if self.triangulation_failures.contains(&key) {
                    Some(AreaRetainedResult::Rejected)
                } else {
                    None
                };
                if let Some(result) = result {
                    self.retained_area_triangulations.admit(
                        AreaRetainedRecord {
                            ordinal,
                            projection,
                            result,
                        },
                        AreaTriangulationRetention::BYTE_CAP,
                        AreaTriangulationRetention::COUNT_CAP,
                    );
                }
            }
            self.retained_area_triangulations.epoch = Some(context.static_area_geometry_epoch());
        }
        tracing::info!("Pre-computed {} area triangulations", count);
    }

    /// Logical retained capacities, not total renderer RSS or original cache size.
    pub fn area_triangulation_reuse_statistics(
        &self,
    ) -> (bool, usize, usize, u64, u64, u64, u64, u64, usize) {
        let r = &self.retained_area_triangulations;
        (
            self.area_triangulation_reuse_enabled,
            r.records.len(),
            r.retained_bytes,
            r.rebind_hits,
            r.rebind_rejections,
            r.precomputed_areas,
            r.cache_hits,
            r.cold_evaluations,
            r.peak_retained_bytes,
        )
    }

    pub fn longitude_wrapping_enabled(&self) -> bool {
        self.lon_wrap_screen_px > 0.0
    }

    pub fn selected_geometry_vertex_count(&self) -> usize {
        self.selection_world_geometry.iter().map(Vec::len).sum()
    }

    pub fn displayed_geometry(&self) -> &[usize] {
        &self.displayed_geometry
    }

    /// Conservative broad-phase candidates in original displayed order.
    /// Stale transforms fall back to the complete current draw list.
    pub fn selection_candidates(
        &self,
        scaler: &ferrite_render::Scaler,
        query: ferrite_render::ScreenPoint,
        radius: f64,
    ) -> Vec<usize> {
        match self.selection_index.get() {
            Some(index) if index.matches_scaler(scaler) => {
                index.candidates(scaler, query, radius, self.longitude_wrapping_enabled())
            }
            _ => self.displayed_geometry.clone(),
        }
    }
    /// Build the spatial broad phase on the first pick, never during navigation.
    /// The exact hit predicates and displayed source order remain authoritative.
    pub fn selection_candidates_in_context(
        &self,
        context: &RenderContext,
        query: ferrite_render::ScreenPoint,
        radius: f64,
    ) -> Vec<usize> {
        if self.triangulation_revision == Some(context.geometry_revision()) {
            self.selection_index.get_or_init(|| {
                let mut index = ferrite_render::SelectionIndex::default();
                let plan = self.line_suppression.current();
                index.rebuild(
                    context.raw_instructions(),
                    &self.displayed_geometry,
                    &context.scaler,
                    |i| plan.and_then(|p| p.spans(i)),
                );
                index
            });
        }
        self.selection_candidates(&context.scaler, query, radius)
    }
    pub fn selection_index_stats(&self) -> &ferrite_render::SelectionIndexStats {
        self.selection_index
            .get()
            .map(|index| index.statistics())
            .unwrap_or(&self.empty_selection_stats)
    }

    pub fn set_selection_geometry(
        &mut self,
        geometry: Vec<Vec<WorldPoint>>,
        scaler: &ferrite_render::Scaler,
    ) {
        self.selection_world_geometry = geometry;
        self.update_selection(scaler);
    }

    pub fn update_selection(&mut self, scaler: &ferrite_render::Scaler) {
        if self.ui_state.selected_feature.is_none() {
            self.selection_world_geometry.clear();
        }

        let shift = self
            .ui_state
            .selected_feature
            .as_ref()
            .map_or(0., |f| f.longitude_shift);
        self.selection_screen_geometry = self
            .selection_world_geometry
            .iter()
            .map(|path| {
                path.iter()
                    .map(|p| {
                        let p = scaler.world_to_screen(WorldPoint::new(p.x + shift, p.y));
                        [p.x, p.y]
                    })
                    .collect()
            })
            .collect();
        self.selection_anchor = self.ui_state.selected_feature.as_ref().map(|f| {
            let p = scaler.world_to_screen(ferrite_render::WorldPoint::new(
                f.world_pos.0 + f.longitude_shift,
                f.world_pos.1,
            ));
            [p.x, p.y]
        });
    }

    /// Upload a product-neutral raster. CPU pixels are released after upload.
    pub fn add_raster_layer(
        &mut self,
        layer: ferrite_render::RasterLayer,
        scaler: &ferrite_render::Scaler,
    ) -> crate::Result<()> {
        let uploaded = self.prepare_raster_layer(layer, scaler)?;
        let mut budget = crate::raster_publication_budget::RasterPublicationBudget::default();
        for layer in &self.raster_layers {
            layer.admit(&mut budget)?;
        }
        uploaded.admit(&mut budget)?;
        self.raster_layers.push(uploaded);
        self.raster_epoch = Arc::new(());

        Ok(())
    }
    pub fn raster_texture_limit(&self) -> u32 {
        self.state.device.limits().max_texture_dimension_2d
    }
    /// Stage GPU textures while releasing each CPU tile. Commit only after the producer succeeds.
    pub fn raster_batch<E: From<crate::WgpuError>>(
        &mut self,
        scaler: &ferrite_render::Scaler,
        replace: bool,
        producer: impl FnOnce(
            &mut dyn FnMut(ferrite_render::RasterLayer) -> std::result::Result<(), E>,
        ) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let batch = self.stage_raster_material_batch(scaler, |upload| {
            producer(&mut |layer| upload(ferrite_render::RasterMaterialLayer::Regular(layer)))
        })?;
        self.commit_raster_material_batch(batch, replace, None)?;
        Ok(())
    }

    /// Bounded transactional staging; the regular raster_batch API is preserved.
    /// Continuous bytes are copied to an unorm texture without premultiplication.
    pub fn stage_raster_material_batch<E: From<crate::WgpuError>>(
        &mut self,
        scaler: &ferrite_render::Scaler,
        producer: impl FnOnce(
            &mut dyn FnMut(ferrite_render::RasterMaterialLayer) -> std::result::Result<(), E>,
        ) -> std::result::Result<(), E>,
    ) -> std::result::Result<PreparedRasterMaterialBatch, E> {
        let mut layers = Vec::new();
        let mut bytes = 0usize;
        let mut budget = crate::raster_publication_budget::RasterPublicationBudget::default();
        producer(&mut |input| {
            let (layer, continuous) = match input {
                ferrite_render::RasterMaterialLayer::Regular(layer) => (layer, None),
                ferrite_render::RasterMaterialLayer::Continuous(layer) => {
                    let (layer, metadata) = layer.into_parts();
                    (layer, Some(metadata))
                }
            };
            bytes = bytes.checked_add(layer.rgba.len()).ok_or_else(|| {
                crate::WgpuError::Render("Raster material batch size overflow".into())
            })?;
            if bytes > 256 * 1024 * 1024 || layers.len() >= 4096 {
                return Err(crate::WgpuError::Render(
                    "Raster material batch exceeds 256MiB/4096-layer budget".into(),
                )
                .into());
            }
            if continuous.is_some() {
                if scaler.projection() != ferrite_render::FlatProjection::LocalGeographic {
                    return Err(crate::WgpuError::Render("Continuous Mercator-domain material requires a qualified inverse-projection bound".into()).into());
                }
                self.pipelines
                    .ensure_continuous_raster_pipeline(&self.state);
            }
            let prepared = self.prepare_raster_material(layer, scaler, continuous)?;
            prepared.admit(&mut budget)?;
            layers.push(prepared);
            Ok(())
        })?;
        Ok(PreparedRasterMaterialBatch {
            layers,
            owner: std::sync::Arc::clone(&self.continuous_owner),
        })
    }
    /// Rebuild only unpublished buffers for the proposed camera. Numerical
    /// continuous qualification still has to bind these actual new bytes.
    pub fn reproject_raster_material_batch(
        &self,
        batch: &mut PreparedRasterMaterialBatch,
        scaler: &ferrite_render::Scaler,
    ) -> Result<()> {
        if !Arc::ptr_eq(&batch.owner, &self.continuous_owner) {
            return Err(WgpuError::Render(
                "Raster batch belongs to another renderer".into(),
            ));
        }
        for layer in &mut batch.layers {
            let vertices = self.raster_vertices(layer.bounds, layer.grid, layer.tile_size, scaler);
            let indices = Self::raster_indices(vertices.len());
            if let Some(identity) = layer.continuous_identity.as_mut() {
                identity.camera = scaler.flat_encoded_identity().ok_or_else(|| {
                    WgpuError::Render("Continuous identity requires an actual flat camera".into())
                })?;
                identity.vertex_bytes = bytemuck::cast_slice(&vertices).to_vec();
                identity.index_bytes = bytemuck::cast_slice(&indices).to_vec();
            }
            layer.vertices = self
                .state
                .create_vertex_buffer(&vertices, "unpublished-raster-vertices");
            layer.indices = self
                .state
                .create_index_buffer(&indices, "unpublished-raster-indices");
            layer.index_count = indices.len() as u32;
        }
        Ok(())
    }
    fn continuous_binding_matches<'a>(
        &self,
        binding: &crate::continuous_frame_binding::ContinuousFrameBinding,
        layers: impl Iterator<Item = &'a GpuRasterLayer>,
    ) -> bool {
        binding.matches_current(
            &self.continuous_owner,
            self.continuous_uniforms
                .iter()
                .flatten()
                .map(bytemuck::bytes_of),
            [self.state.size.width, self.state.size.height],
            crate::state::MSAA_SAMPLE_COUNT,
            self.state.config.format,
            layers.filter_map(|l| l.continuous_identity.as_ref()),
        )
    }
    fn continuous_transform_key(&self) -> [u32; 9] {
        let (width, height) = self.state.viewport_size();
        [
            width.to_bits(),
            height.to_bits(),
            self.screen_pan_offset.0.to_bits(),
            self.screen_pan_offset.1.to_bits(),
            self.screen_zoom_scale.to_bits(),
            self.screen_zoom_scale_y.to_bits(),
            self.screen_zoom_pivot.0.to_bits(),
            self.screen_zoom_pivot.1.to_bits(),
            self.lon_wrap_screen_px.to_bits(),
        ]
    }
    fn raster_publication_frame(&self) -> crate::continuous_frame_binding::ContinuousFrameBinding {
        crate::continuous_frame_binding::ContinuousFrameBinding::capture(
            &self.continuous_owner,
            &self
                .continuous_uniforms
                .iter()
                .flatten()
                .map(|v| bytemuck::bytes_of(v).to_vec())
                .collect::<Vec<_>>(),
            [self.state.size.width, self.state.size.height],
            crate::state::MSAA_SAMPLE_COUNT,
            self.state.config.format,
            self.raster_layers
                .iter()
                .filter_map(|l| l.continuous_identity.clone())
                .collect(),
        )
    }
    /// All recoverable checks and GPU work precede visible scene installation.
    pub fn prepare_raster_material_publication(
        &self,
        batch: PreparedRasterMaterialBatch,
        replace: bool,
        proof: Option<crate::ValidatedContinuousFrame>,
    ) -> crate::Result<PreparedRasterPublication> {
        if !Arc::ptr_eq(&batch.owner, &self.continuous_owner) {
            return Err(crate::WgpuError::Render(
                "Raster material batch belongs to another renderer".into(),
            ));
        }
        let has_continuous = self
            .raster_layers
            .iter()
            .filter(|_| !replace)
            .chain(batch.layers.iter())
            .any(|l| l.continuous.is_some());
        let proof = if has_continuous {
            proof.or_else(|| {
                if !replace {
                    self.continuous_frame.clone()
                } else {
                    None
                }
            })
        } else {
            None
        };
        if has_continuous {
            let certificate=proof.as_ref().ok_or_else(||crate::WgpuError::Render("Continuous selector is staged: validated projection/interpolant/hardware bound required".into()))?;
            if !self.continuous_binding_matches(
                &certificate.binding,
                self.raster_layers
                    .iter()
                    .filter(|_| !replace)
                    .chain(batch.layers.iter()),
            ) || certificate.transform_key != self.continuous_transform_key()
                || !certificate.error().is_finite()
                || certificate.error() < 0.
            {
                return Err(crate::WgpuError::Render(
                    "Continuous frame qualification does not match this viewport".into(),
                ));
            }
            if batch
                .layers
                .iter()
                .filter_map(|layer| layer.continuous)
                .chain(
                    self.raster_layers
                        .iter()
                        .filter(|_| !replace)
                        .filter_map(|layer| layer.continuous),
                )
                .any(|material| !certificate.material_keys.contains(&material.content_digest))
            {
                return Err(crate::WgpuError::Render(
                    "Continuous frame proof does not cover these source/material bytes".into(),
                ));
            }
        }
        let mut budget = crate::raster_publication_budget::RasterPublicationBudget::default();
        let mut count = 0usize;
        for layer in self
            .raster_layers
            .iter()
            .filter(|_| !replace)
            .chain(batch.layers.iter())
        {
            layer.admit(&mut budget)?;
            count += 1;
        }
        // Sharing unchanged regular textures is safe. A retained continuous atlas
        // must be detached before any proof-header write, including requalification.
        let mut layers = Vec::with_capacity(count);
        for live in self.raster_layers.iter().filter(|_| !replace) {
            let mut next = live.clone();
            if live.continuous.is_some() {
                let texture = self.state.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("unpublished-continuous-atlas"),
                    size: live._texture.size(),
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let mut encoder =
                    self.state
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("detach-continuous-atlas"),
                        });
                encoder.copy_texture_to_texture(
                    live._texture.as_image_copy(),
                    texture.as_image_copy(),
                    live._texture.size(),
                );
                self.state.queue.submit(Some(encoder.finish()));
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                next.bind_group = self
                    .pipelines
                    .create_raster_bind_group(&self.state.device, &view);
                next._texture = texture;
            }
            layers.push(next);
        }
        layers.extend(batch.layers);
        if let Some(certificate) = proof.as_ref() {
            for layer in layers.iter().filter(|l| l.continuous.is_some()) {
                let mut bytes = Vec::with_capacity(8);
                bytes.extend(certificate.error().to_bits().to_le_bytes());
                bytes.extend(1u32.to_le_bytes());
                self.state.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &layer._texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 13, y: 0, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &bytes,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(8),
                        rows_per_image: Some(1),
                    },
                    wgpu::Extent3d {
                        width: 2,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        Ok(PreparedRasterPublication {
            layers,
            proof,
            owner: Arc::clone(&self.continuous_owner),
            previous_epoch: Arc::clone(&self.raster_epoch),
            frame: self.raster_publication_frame(),
            transform: self.continuous_transform_key(),
        })
    }
    /// Reproject the entire unpublished regular inventory, including retained
    /// tiles. Continuous sources need a new numerical qualification after this
    /// operation; the current unsupported route cannot reuse its old token.
    pub fn reproject_raster_publication(
        &self,
        prepared: &mut PreparedRasterPublication,
        scaler: &ferrite_render::Scaler,
    ) -> Result<()> {
        self.validate_raster_publication(prepared)?;
        if prepared.layers.iter().any(|l| l.continuous.is_some()) {
            return Err(WgpuError::Render(
                "Continuous camera replacement requires new numerical qualification".into(),
            ));
        }
        for layer in &mut prepared.layers {
            let vertices = self.raster_vertices(layer.bounds, layer.grid, layer.tile_size, scaler);
            let indices = Self::raster_indices(vertices.len());
            layer.vertices = self
                .state
                .create_vertex_buffer(&vertices, "unpublished-raster-scene-vertices");
            layer.indices = self
                .state
                .create_index_buffer(&indices, "unpublished-raster-scene-indices");
            layer.index_count = indices.len() as u32;
        }
        let mut budget = crate::raster_publication_budget::RasterPublicationBudget::default();
        for layer in &prepared.layers {
            layer.admit(&mut budget)?;
        }
        Ok(())
    }
    pub fn validate_raster_publication(
        &self,
        prepared: &PreparedRasterPublication,
    ) -> crate::Result<()> {
        if !raster_publication_identity_matches(
            &self.continuous_owner,
            &prepared.owner,
            &self.raster_epoch,
            &prepared.previous_epoch,
        ) || prepared.transform != self.continuous_transform_key()
            || !self.continuous_binding_matches(&prepared.frame, self.raster_layers.iter())
        {
            return Err(crate::WgpuError::Render(
                "Raster publication renderer, source inventory or view changed".into(),
            ));
        }
        Ok(())
    }
    /// Validate immediately before this infallible installation on the UI thread.
    pub fn commit_raster_publication(&mut self, prepared: PreparedRasterPublication) {
        assert!(
            self.validate_raster_publication(&prepared).is_ok(),
            "Stale raster publication"
        );
        self.raster_layers = prepared.layers;
        self.continuous_frame = prepared.proof;
        self.continuous_layer_count = self
            .raster_layers
            .iter()
            .filter(|l| l.continuous.is_some())
            .count();
        self.raster_epoch = Arc::new(());
    }
    /// Compatibility entry point; no old texture is changed on preparation failure.
    pub fn commit_raster_material_batch(
        &mut self,
        batch: PreparedRasterMaterialBatch,
        replace: bool,
        proof: Option<crate::ValidatedContinuousFrame>,
    ) -> crate::Result<()> {
        let prepared = self.prepare_raster_material_publication(batch, replace, proof)?;
        self.validate_raster_publication(&prepared)?;
        self.commit_raster_publication(prepared);
        Ok(())
    }
    fn validate_continuous_frame(&self) -> crate::Result<()> {
        if self.continuous_layer_count == 0 {
            return Ok(());
        }
        if self.continuous_frame.as_ref().is_none_or(|p| {
            !self.continuous_binding_matches(&p.binding, self.raster_layers.iter())
                || p.transform_key != self.continuous_transform_key()
                || self
                    .raster_layers
                    .iter()
                    .filter_map(|l| l.continuous)
                    .any(|m| !p.material_keys.contains(&m.content_digest))
        }) {
            return Err(crate::WgpuError::Render("Continuous material requires a new qualified frame after viewport/transform change".into()));
        }
        Ok(())
    }
    fn prepare_raster_layer(
        &self,
        layer: ferrite_render::RasterLayer,
        scaler: &ferrite_render::Scaler,
    ) -> crate::Result<GpuRasterLayer> {
        self.prepare_raster_material(layer, scaler, None)
    }
    fn prepare_raster_material(
        &self,
        mut layer: ferrite_render::RasterLayer,
        scaler: &ferrite_render::Scaler,
        continuous: Option<ferrite_render::ContinuousRasterMetadata>,
    ) -> crate::Result<GpuRasterLayer> {
        let required = (layer.width as usize)
            .checked_mul(layer.height as usize)
            .and_then(|n| n.checked_mul(4));
        if layer.width == 0 || layer.height == 0 || required != Some(layer.rgba.len()) {
            return Err(crate::WgpuError::Render("Invalid raster dimensions".into()));
        }
        let limit = self.state.device.limits().max_texture_dimension_2d;
        if layer.width > limit || layer.height > limit {
            return Err(crate::WgpuError::Render(format!(
                "Raster exceeds GPU texture limit {limit}; tiling required"
            )));
        }
        if ![
            layer.bounds.min_x,
            layer.bounds.min_y,
            layer.bounds.max_x,
            layer.bounds.max_y,
        ]
        .iter()
        .all(|v| v.is_finite())
            || layer.bounds.min_x >= layer.bounds.max_x
            || layer.bounds.min_y >= layer.bounds.max_y
        {
            return Err(crate::WgpuError::Render(
                "Invalid raster geographic bounds".into(),
            ));
        }
        let grid = layer.grid.unwrap_or(ferrite_render::RasterGrid {
            bounds: layer.bounds,
            width: layer.width,
            height: layer.height,
            column: 0,
            row: 0,
        });
        let logical_size = continuous
            .map(|m| m.logical_size)
            .unwrap_or([layer.width, layer.height]);
        if grid.width == 0
            || grid.height == 0
            || grid
                .column
                .checked_add(logical_size[0])
                .is_none_or(|x| x > grid.width)
            || grid
                .row
                .checked_add(logical_size[1])
                .is_none_or(|y| y > grid.height)
            || ![
                grid.bounds.min_x,
                grid.bounds.max_x,
                grid.bounds.min_y,
                grid.bounds.max_y,
            ]
            .iter()
            .all(|x| x.is_finite())
            || grid.bounds.min_x >= grid.bounds.max_x
            || grid.bounds.min_y >= grid.bounds.max_y
        {
            return Err(crate::WgpuError::Render(
                "Invalid raster source lattice".into(),
            ));
        }
        // Texture pipeline uses premultiplied alpha.
        if continuous.is_none() {
            for p in layer.rgba.as_chunks_mut::<4>().0 {
                for i in 0..3 {
                    p[i] = ((p[i] as u16 * p[3] as u16 + 127) / 255) as u8;
                }
            }
        }
        let (texture, view) =
            self.state
                .create_texture_from_rgba(&layer.rgba, layer.width, layer.height, &layer.id);
        let bind_group = self
            .pipelines
            .create_raster_bind_group(&self.state.device, &view);
        let vertices = self.raster_vertices(layer.bounds, grid, logical_size, scaler);
        let vb = self
            .state
            .create_vertex_buffer(&vertices, "coverage-raster-vertices");
        let indices = Self::raster_indices(vertices.len());
        let continuous_identity = if let Some(material) = continuous {
            Some(
                crate::continuous_frame_binding::PreparedContinuousIdentity {
                    camera: scaler.flat_encoded_identity().ok_or_else(|| {
                        crate::WgpuError::Render(
                            "Continuous identity requires an actual flat camera".into(),
                        )
                    })?,
                    vertex_bytes: bytemuck::cast_slice(&vertices).to_vec(),
                    index_bytes: bytemuck::cast_slice(&indices).to_vec(),
                    material: material.content_digest,
                },
            )
        } else {
            None
        };
        let ib = self
            .state
            .create_index_buffer(&indices, "coverage-raster-indices");
        Ok(GpuRasterLayer {
            continuous_identity,
            index_count: indices.len() as u32,
            continuous,
            viewing_groups: layer.viewing_groups,
            draw_order: layer.draw_order,
            _texture: texture,
            bind_group,
            bounds: layer.bounds,
            grid,
            tile_size: logical_size,
            vertices: vb,
            indices: ib,
        })
    }
    fn raster_vertices(
        &self,
        bounds: ferrite_render::GeoBounds,
        grid: ferrite_render::RasterGrid,
        tile_size: [u32; 2],
        scaler: &ferrite_render::Scaler,
    ) -> Vec<crate::RasterVertex> {
        let a = scaler.world_to_screen(WorldPoint::new(bounds.min_x, bounds.max_y));
        let b = scaler.world_to_screen(WorldPoint::new(bounds.max_x, bounds.min_y));
        let origin = scaler.world_to_screen(WorldPoint::new(grid.bounds.min_x, grid.bounds.max_y));
        let end = scaler.world_to_screen(WorldPoint::new(grid.bounds.max_x, grid.bounds.min_y));
        let step = [
            (end.x - origin.x) / grid.width as f32,
            (end.y - origin.y) / grid.height as f32,
        ];
        // Extend internal quad edges by one physical pixel. The fragment's global
        // cell test discards other tiles; this prevents partial MSAA coverage at seams.
        let left = if grid.column > 0 { 1. } else { 0. };
        let top = if grid.row > 0 { 1. } else { 0. };
        let right = if grid.column + tile_size[0] < grid.width {
            1.
        } else {
            0.
        };
        let bottom = if grid.row + tile_size[1] < grid.height {
            1.
        } else {
            0.
        };
        let vertex = |x, y, dx, dy| crate::RasterVertex {
            position: [x + dx, y + dy],
            anchor: [x, y],
            origin: [origin.x, origin.y],
            step,
            offset: [grid.column, grid.row],
            size: [grid.width, grid.height],
            row_bounds: [0., 0.],
            row_index: u32::MAX,
            padding: 0,
        };
        if scaler.projection() == ferrite_render::FlatProjection::LocalGeographic {
            return vec![
                vertex(a.x, a.y, -left, -top),
                vertex(b.x, a.y, right, -top),
                vertex(b.x, b.y, right, bottom),
                vertex(a.x, b.y, -left, bottom),
            ];
        }
        // At most one strip per source row: exact cell ownership at all zooms.
        let mut result = Vec::with_capacity(tile_size[1] as usize * 4);
        for local in 0..tile_size[1] {
            let row = grid.row + local;
            let latitude = |row: u32| {
                grid.bounds.max_y - grid.bounds.height() * row as f64 / grid.height as f64
            };
            let y0 = scaler
                .world_to_screen(WorldPoint::new(bounds.min_x, latitude(row)))
                .y;
            let y1 = scaler
                .world_to_screen(WorldPoint::new(bounds.min_x, latitude(row + 1)))
                .y;
            let pad_top = if row > 0 { 1. } else { 0. };
            let pad_bottom = if row + 1 < grid.height { 1. } else { 0. };
            for mut v in [
                vertex(a.x, y0, -left, -pad_top),
                vertex(b.x, y0, right, -pad_top),
                vertex(b.x, y1, right, pad_bottom),
                vertex(a.x, y1, -left, pad_bottom),
            ] {
                v.row_bounds = [y0, y1];
                v.row_index = row;
                result.push(v);
            }
        }
        result
    }
    fn raster_indices(vertices: usize) -> Vec<u32> {
        (0..vertices / 4)
            .flat_map(|q| {
                let b = q as u32 * 4;
                [b, b + 1, b + 2, b, b + 2, b + 3]
            })
            .collect()
    }
    pub fn update_raster_view(&mut self, scaler: &ferrite_render::Scaler) {
        self.raster_epoch = Arc::new(());
        self.continuous_frame = None;
        for i in 0..self.raster_layers.len() {
            let vertices = self.raster_vertices(
                self.raster_layers[i].bounds,
                self.raster_layers[i].grid,
                self.raster_layers[i].tile_size,
                scaler,
            );
            let count = (vertices.len() / 4 * 6) as u32;
            if count != self.raster_layers[i].index_count {
                self.raster_layers[i].indices = self.state.create_index_buffer(
                    &Self::raster_indices(vertices.len()),
                    "coverage-raster-indices",
                );
                self.raster_layers[i].index_count = count;
            }
            self.raster_layers[i].vertices = self
                .state
                .create_vertex_buffer(&vertices, "coverage-raster-vertices");
        }
    }
    /// Update visibility without re-uploading coverage pixels or geometry.
    pub fn set_raster_enabled_groups(&mut self, groups: Option<&std::collections::HashSet<u32>>) {
        self.raster_enabled_groups = groups.cloned();
        self.raster_epoch = Arc::new(());
    }
    /// Inspect retained GPU layer composition without copying textures or pixel buffers.
    pub fn raster_composition_metadata(
        &self,
    ) -> impl Iterator<Item = (ferrite_kernel::CompositionPlane, i32, &[u32], bool)> {
        self.raster_layers.iter().map(|layer| {
            let (plane, priority) = layer.draw_order.render_key();
            (
                plane,
                priority,
                layer.viewing_groups.as_slice(),
                ferrite_render::raster_groups_visible(
                    &layer.viewing_groups,
                    self.raster_enabled_groups.as_ref(),
                ),
            )
        })
    }
    pub fn clear_raster_layers(&mut self) {
        self.raster_epoch = Arc::new(());
        self.continuous_frame = None;
        self.continuous_layer_count = 0;
        self.raster_layers.clear();
    }
    fn draw_rasters<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        view: &'a wgpu::BindGroup,
        key: (CompositionPlane, i32),
        draw_index: Option<&DrawRangeIndex>,
    ) {
        if self.raster_layers.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipelines.raster_pipeline);
        pass.set_bind_group(0, view, &[]);
        for layer_index in
            selected_indices(draw_index, DrawKind::Raster, key, self.raster_layers.len())
        {
            let layer = &self.raster_layers[layer_index];
            if !(layer.draw_order.render_key() == key
                && ferrite_render::raster_groups_visible(
                    &layer.viewing_groups,
                    self.raster_enabled_groups.as_ref(),
                ))
            {
                continue;
            }
            pass.set_pipeline(if layer.continuous.is_some() {
                self.pipelines
                    .continuous_raster_pipeline
                    .as_ref()
                    .expect("staged continuous pipeline")
            } else {
                &self.pipelines.raster_pipeline
            });
            pass.set_bind_group(1, &layer.bind_group, &[]);
            pass.set_vertex_buffer(0, layer.vertices.slice(..));
            pass.set_index_buffer(layer.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..layer.index_count, 0, 0..1);
        }
    }

    /// Clear symbol textures (call when color profile changes)
    pub fn clear_symbol_textures(&mut self) {
        self.symbol_textures.clear();
        // Patterns are rasterized with the same PC colour profile as symbols.
        self.pattern_textures.clear();
    }

    /// Exactly the quad/culling decision used by both packing and Parent
    /// execution evidence. Longitude copies must be considered before culling.
    fn symbol_quad(&self, instance: &SymbolInstance) -> Option<[TextureVertex; 4]> {
        let tex = self.symbol_textures.get(&instance.symbol_id)?;
        if !tex.has_coverage {
            return None;
        }
        let (vp_w, vp_h) = self.state.viewport_size();
        let sym_min_x = -200.;
        let sym_min_y = -200.;
        let sym_max_x = vp_w + 200.;
        let sym_max_y = vp_h + 200.;
        let display_scale = instance.scale / tex.render_scale
            * self.symbol_scale
            * self.state.window.scale_factor() as f32;
        let half_w = (tex.width as f32 * display_scale) / 2.0;
        let half_h = (tex.height as f32 * display_scale) / 2.0;
        let pivot_x = tex.pivot_in_texture.0 * display_scale;
        let pivot_y = tex.pivot_in_texture.1 * display_scale;
        let rotation = instance.rotation.to_radians();
        let cos_r = rotation.cos();
        let sin_r = rotation.sin();

        let transform = |dx: f32, dy: f32| -> (f32, f32) {
            let px = dx + half_w - pivot_x;
            let py = dy + half_h - pivot_y;
            let rx = px * cos_r - py * sin_r;
            let ry = px * sin_r + py * cos_r;
            (instance.screen_x + rx, instance.screen_y + ry)
        };

        let (x0, y0) = transform(-half_w, -half_h);
        let (x1, y1) = transform(half_w, -half_h);
        let (x2, y2) = transform(half_w, half_h);
        let (x3, y3) = transform(-half_w, half_h);

        let corners = [(x0, y0), (x1, y1), (x2, y2), (x3, y3)];
        if !corners.iter().all(|(x, y)| x.is_finite() && y.is_finite()) {
            return None;
        }
        let offsets = [0., -self.lon_wrap_screen_px, self.lon_wrap_screen_px];
        let copies = if self.lon_wrap_screen_px > 0.
            && !instance
                .source
                .is_some_and(|s| self.device_fixed_sources.contains(&s))
        {
            3
        } else {
            1
        };
        let visible = offsets[..copies].iter().any(|offset| {
            !corners.iter().all(|(x, _)| x + offset < sym_min_x)
                && !corners.iter().all(|(x, _)| x + offset > sym_max_x)
                && !corners.iter().all(|(_, y)| *y < sym_min_y)
                && !corners.iter().all(|(_, y)| *y > sym_max_y)
        });
        if !visible {
            return None;
        }
        Some([
            TextureVertex::new(x0, y0, 0., 0., instance.anchor),
            TextureVertex::new(x1, y1, 1., 0., instance.anchor),
            TextureVertex::new(x2, y2, 1., 1., instance.anchor),
            TextureVertex::new(x3, y3, 0., 1., instance.anchor),
        ])
    }

    /// Pack symbol instances into contiguous vertex/index arrays for single-buffer rendering.
    /// Produces packed_symbol_vertices, packed_symbol_indices, and packed_symbol_ranges.
    fn pack_symbol_batch_range(&mut self, start: usize, end: usize) {
        self.packed_symbol_vertices.clear();
        self.packed_symbol_indices.clear();
        self.packed_symbol_ranges.clear();

        // Pack in portrayal order; merge only consecutive equal textures.
        for instance in &self.symbol_instances[start..end] {
            if let Some(quad) = self.symbol_quad(instance) {
                let vertex_offset = self.packed_symbol_vertices.len() as u32;
                let index_start = self.packed_symbol_indices.len() as u32;
                self.packed_symbol_vertices.extend_from_slice(&quad);
                self.packed_symbol_indices.extend_from_slice(&[
                    vertex_offset,
                    vertex_offset + 1,
                    vertex_offset + 2,
                    vertex_offset,
                    vertex_offset + 2,
                    vertex_offset + 3,
                ]);
                if let Some((id, start, count)) = self.packed_symbol_ranges.last_mut() {
                    if *id == instance.symbol_id && *start + *count == index_start {
                        *count += 6;
                        continue;
                    }
                }
                self.packed_symbol_ranges
                    .push((instance.symbol_id, index_start, 6));
            }
        }
    }

    /// Actual draw ranges; consecutive identical textures share a range.
    /// Explicit diagnostic export; never called by the interactive frame loop.
    pub fn audit_geometry_buffers(&self, directory: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(directory)?;
        macro_rules! buffer {
            ($name:literal, $values:expr) => {
                std::fs::write(
                    directory.join(concat!($name, ".bin")),
                    bytemuck::cast_slice($values),
                )?;
            };
        }
        buffer!("area-vertices", &self.area_vertices);
        buffer!("area-indices", &self.area_indices);
        buffer!("line-vertices", &self.line_vertices);
        buffer!("line-indices", &self.line_indices);
        buffer!("symbol-vertices", &self.packed_symbol_vertices);
        buffer!("symbol-indices", &self.packed_symbol_indices);
        buffer!("pattern-vertices", &self.pattern_vertices);
        buffer!("pattern-indices", &self.pattern_indices);
        let metadata = serde_json::json!({
            "geometry_transform": self.geometry_transform,
            "fast_view_transform": self.fast_view_transform(),
            "fast_view_scales": self.fast_view_scales(),
            "area_ranges": self.area_priority_ranges.iter().map(|&(p,q,a,b,_)| (p,q,a,b)).collect::<Vec<_>>(),
            "line_ranges": self.line_priority_ranges.iter().map(|&(p,q,a,b,_)| (p,q,a,b)).collect::<Vec<_>>(),
            "pattern_emissions": self.pattern_emission_audit.as_ref().map(|records| records.iter().map(|r| serde_json::json!({
                "source_ordinal":r.source_ordinal,"vertex_start":r.vertex_start,"vertex_end":r.vertex_end,
                "index_start":r.index_start,"index_end":r.index_end,"wrap_mode":r.wrap_mode,
                "wrap_dx_screen_bits":r.wrap_dx_screen_bits,
            })).collect::<Vec<_>>()),
            "pattern_emission_dropped": self.pattern_emission_audit_dropped,
            "pattern_emission_scope": "full appended CPU vertex ownership including unused earcut vertices; wrap255 mesh is subsequently reused by draw-time wrap uniforms",
            "pattern_ranges": self.pattern_ranges.iter().map(|(p,q,a,b,k,w,_)| (p,q,a,b,k,w)).collect::<Vec<_>>(),
            "packed_symbol_ranges": self.packed_symbol_ranges.iter().map(|&(id,start,count)| (ferrite_render::resolve_symbol(id),start,count)).collect::<Vec<_>>(),
            "symbol_gpu_buffers": self.symbol_gpu_buffer_statistics(),
            "symbol_buffer_scope": "last packed plane/priority range; complete displayed symbols are in snapshot.json",
            "displayed_geometry": self.displayed_geometry,
            "selection_index": self.selection_index_stats(),
            "drawing_dependencies": self.dependency_status.audit_value(),
        });
        std::fs::write(
            directory.join("metadata.json"),
            serde_json::to_vec_pretty(&metadata)?,
        )
    }

    pub fn symbol_gpu_buffer_statistics(&self) -> serde_json::Value {
        serde_json::json!({
            "instancing_enabled": self.pipelines.symbol_instance_pipeline.is_some(),
            "vertex_bytes": self.cached_symbol_buffers.iter().map(|b| b.4.size()).sum::<u64>(),
            "index_bytes": if self.pipelines.symbol_instance_pipeline.is_some() { self.shared_symbol_quad_index_buffer.as_ref().map_or(0,wgpu::Buffer::size) } else { self.cached_symbol_buffers.iter().map(|b| b.5.size()).sum::<u64>() },
            "shared_quad_index_buffers": usize::from(self.shared_symbol_quad_index_buffer.is_some()),
            "buffer_pairs": self.cached_symbol_buffers.len(),
            "instance_payload_bytes": 40,
            "original_payload_per_symbol_bytes": 120,
            "immutable_instance_cache": self.immutable_instance_cache.as_ref().map(|cache|serde_json::json!({
                "requests":cache.requests,"hits":cache.hits,"creations":cache.creations,"rejected":cache.rejected,"resets":cache.resets,
                "entries":cache.entries(),"retained_logical_bytes":cache.retained_bytes(),
                "budget_bytes":crate::immutable_payload_cache::ImmutablePayloadCache::<wgpu::Buffer>::BUDGET,
                "no_live_buffer_writes":true,"scope":"cache-owned immutable CPU/GPU payload budget; live cloned handles and total GPU/RSS excluded",
            })),
            "scope": "2D packed symbols",
        })
    }

    pub fn symbol_draw_batch_count(&self) -> usize {
        self.cached_symbol_buffers
            .iter()
            .map(|batch| batch.6.len())
            .sum()
    }

    /// Symbols accepted by the same visibility and collision filters as drawing.
    pub fn displayed_symbols(&self) -> Vec<DisplayedSymbol> {
        self.symbol_instances
            .iter()
            .map(|s| {
                (
                    ferrite_render::resolve_symbol(s.symbol_id),
                    s.feature_id,
                    s.world,
                    ferrite_render::ScreenPoint::new(s.screen_x, s.screen_y),
                    s.priority,
                    s.cell_index,
                    s.plane,
                )
            })
            .collect()
    }

    /// Symbols accepted by the same visibility and collision filters as drawing.
    pub fn displayed_symbols_with_sources(&self) -> Vec<DisplayedSymbolWithSource> {
        self.symbol_instances
            .iter()
            .map(|s| {
                (
                    ferrite_render::resolve_symbol(s.symbol_id),
                    s.feature_id,
                    s.world,
                    ferrite_render::ScreenPoint::new(s.screen_x, s.screen_y),
                    s.priority,
                    s.cell_index,
                    s.plane,
                    s.source,
                )
            })
            .collect()
    }

    /// Chart rectangle in physical pixels, matching GPU and pointer coordinates.
    pub fn chart_viewport_pixels(&self) -> (f32, f32, f32, f32) {
        let (x, y, width, height) = self.ui_state.chart_area;
        let pixels_per_point = self.egui.ctx.pixels_per_point();
        (
            x * pixels_per_point,
            y * pixels_per_point,
            width * pixels_per_point,
            height * pixels_per_point,
        )
    }

    pub fn ui_pixels_per_point(&self) -> f32 {
        self.egui.ctx.pixels_per_point()
    }
    pub fn fast_view_transform(&self) -> ((f32, f32), f32, (f32, f32)) {
        (
            self.screen_pan_offset,
            self.screen_zoom_scale,
            self.screen_zoom_pivot,
        )
    }

    /// Handle window resize
    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.state.resize(new_size);
        self.update_view_uniforms();
    }

    /// Handle winit window event for egui, returns true if egui consumed the event
    pub fn handle_egui_event(&mut self, event: &WindowEvent) -> bool {
        self.egui.handle_event(&self.state.window, event)
    }

    /// Check if egui wants pointer input (mouse is over UI element)
    /// Call this before handling clicks to avoid clicking through UI
    pub fn egui_wants_pointer(&self) -> bool {
        self.egui.wants_pointer_input()
    }

    /// Check if egui has requested a repaint (e.g., animations, hover effects)
    #[inline]
    pub fn egui_needs_repaint(&self) -> bool {
        self.egui.ctx.has_requested_repaint()
    }

    /// Update cursor position in UI state (world coordinates)
    #[inline]
    pub fn set_cursor_world(&mut self, x: f64, y: f64) {
        self.ui_state.cursor_world = (x, y);
    }

    /// Update cursor position in UI state (screen coordinates)
    #[inline]
    pub fn set_cursor_screen(&mut self, x: f32, y: f32) {
        self.ui_state.cursor_screen = (x, y);
    }

    /// Check and clear UI action requests
    #[inline]
    pub fn take_open_file_request(&mut self) -> bool {
        let requested = self.ui_state.open_file_requested;
        self.ui_state.open_file_requested = false;
        requested
    }

    #[inline]
    pub fn take_screenshot_request(&mut self) -> bool {
        let requested = self.ui_state.screenshot_requested;
        self.ui_state.screenshot_requested = false;
        requested
    }

    #[inline]
    pub fn take_open_fc_request(&mut self) -> bool {
        let requested = self.ui_state.open_fc_requested;
        self.ui_state.open_fc_requested = false;
        requested
    }

    #[inline]
    pub fn take_open_pc_request(&mut self) -> bool {
        let requested = self.ui_state.open_pc_requested;
        self.ui_state.open_pc_requested = false;
        requested
    }

    #[inline]
    pub fn take_zoom_in_request(&mut self) -> bool {
        let requested = self.ui_state.zoom_in_requested;
        self.ui_state.zoom_in_requested = false;
        requested
    }

    #[inline]
    pub fn take_zoom_out_request(&mut self) -> bool {
        let requested = self.ui_state.zoom_out_requested;
        self.ui_state.zoom_out_requested = false;
        requested
    }

    #[inline]
    pub fn take_reset_view_request(&mut self) -> bool {
        let requested = self.ui_state.reset_view_requested;
        self.ui_state.reset_view_requested = false;
        requested
    }

    /// Take and reset clear charts request
    #[inline]
    pub fn take_clear_charts_request(&mut self) -> bool {
        let requested = self.ui_state.clear_charts_requested;
        self.ui_state.clear_charts_requested = false;
        requested
    }

    /// Take color profile change request, returns new profile name if changed
    #[inline]
    pub fn take_color_profile_change(&mut self) -> Option<String> {
        if self.ui_state.color_profile_changed {
            self.ui_state.color_profile_changed = false;
            Some(self.ui_state.color_profile.clone())
        } else {
            None
        }
    }

    /// Set the current color profile name in UI state
    #[inline]
    pub fn set_color_profile(&mut self, profile: &str) {
        self.ui_state.color_profile = profile.to_string();
    }

    /// Take settings change request, returns current settings if changed
    #[inline]
    pub fn take_settings_change(&mut self) -> Option<SettingsState> {
        if self.ui_state.settings_changed {
            self.ui_state.settings_changed = false;
            Some(self.ui_state.settings.clone())
        } else {
            None
        }
    }

    /// Take pan adjustment (in pixels) when panel state changes
    #[inline]
    pub fn take_pan_adjust_pixels(&mut self) -> Option<f32> {
        self.ui_state
            .pan_adjust_pixels
            .take()
            .map(|adjust| adjust * self.egui.ctx.pixels_per_point())
    }

    /// Get current settings state (read-only)
    #[inline]
    pub fn settings(&self) -> &SettingsState {
        &self.ui_state.settings
    }

    /// Update settings state
    #[inline]
    pub fn set_settings(&mut self, settings: SettingsState) {
        self.ui_state.settings = settings;
    }

    /// Take plugin toggle request, returns plugin_id if a toggle was requested
    #[inline]
    pub fn take_plugin_toggle_request(&mut self) -> Option<String> {
        self.ui_state.plugin_toggle_requested.take()
    }

    /// Update plugin toolbar buttons
    #[inline]
    pub fn set_plugin_buttons(&mut self, buttons: Vec<egui_integration::PluginButton>) {
        self.ui_state.plugin_buttons = buttons;
    }

    /// Update plugin UI data
    #[inline]
    pub fn set_plugin_ui_data(&mut self, data: Vec<(String, String)>) {
        self.ui_state.plugin_ui_data = data;
    }

    /// Take pending plugin UI events
    #[inline]
    pub fn take_plugin_ui_events(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.ui_state.plugin_ui_events)
    }

    /// Update view uniforms after resize or zoom
    fn update_view_uniforms(&mut self) {
        let (width, height) = self.state.viewport_size();
        let mut uniforms = ViewUniforms::with_pan_zoom(
            width,
            height,
            1.0,
            self.screen_pan_offset.0,
            self.screen_pan_offset.1,
            self.screen_zoom_scale,
            self.screen_zoom_pivot.0,
            self.screen_zoom_pivot.1,
        );
        uniforms.zoom_scale_y = self.screen_zoom_scale_y;
        self.continuous_uniforms = [Some(uniforms), None, None];
        self.state
            .update_view_uniforms(&self.view_buffer, &uniforms);

        // Update wrapping view uniforms for ±360° longitude copies
        if self.lon_wrap_screen_px > 0.0 {
            let mut left = ViewUniforms::with_pan_zoom(
                width,
                height,
                1.0,
                self.screen_pan_offset.0 - self.lon_wrap_screen_px,
                self.screen_pan_offset.1,
                self.screen_zoom_scale,
                self.screen_zoom_pivot.0,
                self.screen_zoom_pivot.1,
            );
            left.zoom_scale_y = self.screen_zoom_scale_y;
            self.continuous_uniforms[1] = Some(left);
            self.state
                .update_view_uniforms(&self.view_buffer_left, &left);

            let mut right = ViewUniforms::with_pan_zoom(
                width,
                height,
                1.0,
                self.screen_pan_offset.0 + self.lon_wrap_screen_px,
                self.screen_pan_offset.1,
                self.screen_zoom_scale,
                self.screen_zoom_pivot.0,
                self.screen_zoom_pivot.1,
            );
            right.zoom_scale_y = self.screen_zoom_scale_y;
            self.continuous_uniforms[2] = Some(right);
            self.state
                .update_view_uniforms(&self.view_buffer_right, &right);
        }
        self.update_coverage_transform();
    }

    fn coverage_clip_transform(&self) -> Result<crate::coverage_clip::ClipTransform> {
        let [sx, sy] = [self.screen_zoom_scale, self.screen_zoom_scale_y];
        let (px, py) = self.screen_zoom_pivot;
        crate::coverage_clip::ClipTransform::new(
            [1. / sx, 1. / sy],
            [
                px - px / sx - self.screen_pan_offset.0,
                py - py / sy - self.screen_pan_offset.1,
            ],
        )
    }

    /// Transform a retained symbol anchor exactly as its vertex shader does.
    pub fn displayed_symbol_screen(&self, anchor: [f32; 2], pass: usize) -> Option<[f32; 2]> {
        let wrap = match pass {
            0 => 0.,
            1 if self.longitude_wrapping_enabled() => -self.lon_wrap_screen_px,
            2 if self.longitude_wrapping_enabled() => self.lon_wrap_screen_px,
            _ => return None,
        };
        let (px, py) = self.screen_zoom_pivot;
        let point = [
            (anchor[0] + self.screen_pan_offset.0 + wrap - px) * self.screen_zoom_scale + px,
            (anchor[1] + self.screen_pan_offset.1 - py) * self.screen_zoom_scale_y + py,
        ];
        point.iter().all(|v| v.is_finite()).then_some(point)
    }

    /// Sample the retained draw mask using the same inverse affine as the GPU.
    pub fn coverage_fragment_visible(&self, index: usize, pass: usize, pixel: [f32; 2]) -> bool {
        if self.coverage_failed
            || (pass != 0 && self.device_fixed_sources.contains(&index))
            || !pixel.iter().all(|v| v.is_finite())
        {
            return false;
        }
        let Some(prepared) = &self.prepared_coverage else {
            return true;
        };
        let Some(frame) = &self.coverage_frame else {
            return false;
        };
        if frame.resolve_instruction(prepared, pass, index).is_err() {
            return false;
        }
        let Ok(transform) = self.coverage_clip_transform() else {
            return false;
        };
        let point = transform.prepared_pixel(pixel).map(f64::from);
        prepared
            .pass(pass)
            .and_then(|p| p.accepts_fragment(index, point))
            .unwrap_or(false)
    }

    fn update_coverage_transform(&mut self) {
        let transform = self.coverage_clip_transform();
        if let Some(frame) = &mut self.coverage_frame {
            match transform {
                Ok(transform) => frame.set_transform(&self.state.queue, transform),
                Err(error) => {
                    self.coverage_failed = true;
                    tracing::error!("Coverage affine rejected: {error}");
                }
            }
        }
    }

    /// Apply an affine only when all retained geometry supports navigation.
    /// Validate both directions before changing uniforms or coverage state.
    fn accepts_gpu_navigation(&self, scale: [f32; 2], pan: [f32; 2], pivot: [f32; 2]) -> bool {
        if false || self.requires_visibility_rebuild_for_navigation() {
            return false;
        }
        (0..2).all(|axis| {
            let (s, d, p) = (scale[axis], pan[axis], pivot[axis]);
            s.is_finite()
                && s > 0.
                && d.is_finite()
                && p.is_finite()
                && (1. / s).is_finite()
                && ((d - p) * s + p).is_finite()
                && (p - p / s - d).is_finite()
        })
    }

    /// Set screen-space pan. False means the caller must rebuild geometry;
    /// retained geometry and uniforms remain unchanged on rejection.
    #[inline]
    pub fn set_pan_offset(&mut self, dx: f32, dy: f32) -> bool {
        if !self.accepts_gpu_navigation(
            [self.screen_zoom_scale, self.screen_zoom_scale_y],
            [dx, dy],
            [self.screen_zoom_pivot.0, self.screen_zoom_pivot.1],
        ) {
            return false;
        }
        self.screen_pan_offset = (dx, dy);
        self.update_view_uniforms();
        true
    }

    /// Add a pan, returning None when geometry must be rebuilt or the
    /// composed transform is invalid. Rejection never changes the old offset.
    pub fn add_pan_offset(&mut self, dx: f32, dy: f32) -> Option<(f32, f32)> {
        let pan = (self.screen_pan_offset.0 + dx, self.screen_pan_offset.1 + dy);
        self.set_pan_offset(pan.0, pan.1).then_some(pan)
    }

    /// Get the current screen-space pan offset
    #[inline]
    pub fn get_pan_offset(&self) -> (f32, f32) {
        self.screen_pan_offset
    }

    /// Reset pan offset to zero (call before full rebuild)
    #[inline]
    pub fn reset_pan_offset(&mut self) {
        self.screen_pan_offset = (0.0, 0.0);
        self.screen_zoom_scale = 1.0;
        self.screen_zoom_scale_y = 1.0;
        self.screen_zoom_pivot = (0.0, 0.0);
        self.update_view_uniforms();
    }

    /// Set GPU zoom only for geometry that supports an affine navigation.
    /// False means a full rebuild is required; no state changes on rejection.
    #[inline]
    pub fn set_gpu_zoom(&mut self, scale: f32, pivot_x: f32, pivot_y: f32) -> bool {
        if !self.accepts_gpu_navigation(
            [scale, scale],
            [self.screen_pan_offset.0, self.screen_pan_offset.1],
            [pivot_x, pivot_y],
        ) {
            return false;
        }
        self.screen_zoom_scale = scale;
        self.screen_zoom_scale_y = scale;
        self.screen_zoom_pivot = (pivot_x, pivot_y);
        self.update_view_uniforms();
        true
    }

    /// Map the existing geometry to exactly the same scaler used by the next
    /// rebuild. Repeated scroll pivots replace an affine transform rather than
    /// accumulating scale/pan around a stale cursor.
    /// Navigation can change date/scale-independent clipping and collisions,
    /// which in turn can change Parent execution. Re-evaluate those display
    /// lists rather than reuse a stale fast-path permission mask.
    pub fn requires_visibility_rebuild_for_navigation(&self) -> bool {
        self.dependency_status.iterations > 0
            || self.view_dependent_symbols
            || self.view_clipped_patterns
    }

    pub fn begin_flat_diagnostics(
        &mut self,
        frame: u64,
        source_epoch: u64,
        view_epoch: u64,
    ) -> Result<()> {
        if !crate::background_test::enabled()
            || self.window().is_visible().unwrap_or(true)
            || self.window().has_focus()
        {
            return Err(WgpuError::Render(
                "Flat diagnostics require hidden unfocused background mode".into(),
            ));
        }
        self.flat_gpu_coverage_host_ns = 0;
        self.flat_diagnostic = Some(std::rc::Rc::new(std::cell::RefCell::new(
            ferrite_render::flat_reuse_diagnostics::FlatFrameSample::new(
                frame,
                source_epoch,
                view_epoch,
                true,
                false,
            ),
        )));
        Ok(())
    }
    pub fn finish_flat_diagnostics(
        &mut self,
        service_ns: u64,
    ) -> Result<ferrite_render::flat_reuse_diagnostics::FlatFrameSample> {
        if self.window().is_visible().unwrap_or(true) || self.window().has_focus() {
            return Err(WgpuError::Render(
                "Flat diagnostic window visibility changed".into(),
            ));
        }
        let cell = self
            .flat_diagnostic
            .take()
            .ok_or_else(|| WgpuError::Render("No diagnostic frame".into()))?;
        let mut row = *cell.borrow();
        row.service_ns = service_ns;
        row.work.dependency_iterations = self.dependency_status.iterations as u64;
        Ok(row)
    }
    fn flat_span(
        &self,
        stage: ferrite_render::flat_reuse_diagnostics::FlatFrameStage,
    ) -> Option<FlatDiagnosticSpan> {
        self.flat_diagnostic
            .as_ref()
            .map(|cell| FlatDiagnosticSpan {
                cell: cell.clone(),
                stage,
                start: std::time::Instant::now(),
            })
    }
    pub fn flat_gpu_coverage_host_ns(&self) -> u64 {
        self.flat_gpu_coverage_host_ns
    }
    /// (enabled, alias bindings, unique clips, legacy logical R8 charge, unique R8 payload).
    /// Current frame only; no cache identity or resources survive replacement.
    pub fn coverage_clip_cse_statistics(&self) -> (bool, usize, usize, usize, usize) {
        self.coverage_frame.as_ref().map_or(
            (self.frame_local_coverage_clip_reuse, 0, 0, 0, 0),
            |frame| {
                (
                    self.frame_local_coverage_clip_reuse,
                    frame.mask_count(),
                    frame.unique_clip_count(),
                    frame.pixel_bytes(),
                    frame.unique_pixel_bytes(),
                )
            },
        )
    }

    pub fn flat_diagnostics_active(&self) -> bool {
        self.flat_diagnostic.is_some()
    }
    pub fn record_flat_stage(
        &self,
        stage: ferrite_render::flat_reuse_diagnostics::FlatFrameStage,
        elapsed: std::time::Duration,
    ) {
        if let Some(cell) = &self.flat_diagnostic {
            cell.borrow_mut()
                .record_span(stage, elapsed.as_nanos().min(u64::MAX as u128) as u64);
        }
    }
    fn record_flat_reuse(
        &self,
        affine: Option<ferrite_render::flat_reuse_diagnostics::NavigationAffine>,
    ) {
        if let Some(cell) = &self.flat_diagnostic {
            use ferrite_render::flat_reuse_diagnostics::*;
            let reason = classify_flat_reuse(FlatNavigationInputs {
                mode_supported: true,
                dependency_iterations: self.dependency_status.iterations,
                view_dependent_symbols: self.view_dependent_symbols,
                view_clipped_patterns: self.view_clipped_patterns,
                source_transform_present: self.geometry_transform.is_some(),
                affine,
            });
            cell.borrow_mut().record_reuse(reason);
        }
    }
    pub fn set_gpu_view_scaler(&mut self, target: &ferrite_render::Scaler) -> bool {
        if false || self.requires_visibility_rebuild_for_navigation() {
            self.record_flat_reuse(None);
            return false;
        }
        let Some(source) = self.geometry_transform else {
            self.record_flat_reuse(None);
            return false;
        };
        let Some(view) = ferrite_render::ScreenAffine::between_transform(source, target) else {
            self.record_flat_reuse(None);
            return false;
        };
        let pan = [
            view.translation[0] / view.scale[0],
            view.translation[1] / view.scale[1],
        ];
        self.record_flat_reuse(Some(
            ferrite_render::flat_reuse_diagnostics::NavigationAffine {
                scale: view.scale,
                pan,
                pivot: [0., 0.],
            },
        ));
        if !self.accepts_gpu_navigation(view.scale, pan, [0., 0.]) {
            return false;
        }
        self.screen_zoom_scale = view.scale[0];
        self.screen_zoom_scale_y = view.scale[1];
        self.screen_pan_offset = (
            view.translation[0] / view.scale[0],
            view.translation[1] / view.scale[1],
        );
        self.screen_zoom_pivot = (0., 0.);
        self.update_view_uniforms();
        true
    }

    pub fn fast_view_scales(&self) -> (f32, f32) {
        (self.screen_zoom_scale, self.screen_zoom_scale_y)
    }

    /// Get current GPU zoom scale
    #[inline]
    pub fn gpu_zoom_scale(&self) -> f32 {
        self.screen_zoom_scale
    }

    /// Begin a new frame - clears buffers
    pub fn begin_frame(&mut self) {
        self.begin_frame_ex(false);
    }

    /// Begin a new frame with optional preservation of declutter state
    /// If `preserve_declutter` is true, skip screen-space decluttering during animation
    pub fn begin_frame_ex(&mut self, preserve_declutter: bool) {
        // Mark GPU buffers as needing rebuild
        self.gpu_buffers_dirty = true;
        // Invalidate cached symbol GPU buffers (geometry changed)
        self.cached_symbol_buffers.clear();

        // Preserve previous frame counts for pre-allocation (avoids realloc during build)
        let prev_area_v = self.area_vertices.len();
        let prev_area_i = self.area_indices.len();
        let prev_line_v = self.line_vertices.len();
        let prev_line_i = self.line_indices.len();

        self.area_vertices.clear();
        self.area_indices.clear();
        self.line_vertices.clear();
        self.line_indices.clear();
        self.symbol_instances.clear();
        self.displayed_geometry.clear();
        self.selection_index.take();
        self.dependency_status = DependencyRenderStatus::default();

        // Reserve capacity based on previous frame (amortized zero reallocs in steady state)
        if prev_area_v > self.area_vertices.capacity() / 2 {
            self.area_vertices.reserve(prev_area_v);
        }
        if prev_area_i > self.area_indices.capacity() / 2 {
            self.area_indices.reserve(prev_area_i);
        }
        if prev_line_v > self.line_vertices.capacity() / 2 {
            self.line_vertices.reserve(prev_line_v);
        }
        if prev_line_i > self.line_indices.capacity() / 2 {
            self.line_indices.reserve(prev_line_i);
        }
        // Clear symbol batches for new frame
        // Clear priority ranges for S-101 compliant rendering
        self.device_fixed_sources.clear();
        self.prepared_coverage = None;
        self.coverage_frame = None;
        self.coverage_failed = false;
        self.emitting_coverage_source = None;
        self.area_priority_ranges.clear();
        self.line_priority_ranges.clear();
        self.symbol_priority_ranges.clear();
        self.pattern_vertices.clear();
        self.pattern_indices.clear();
        self.pattern_ranges.clear();
        if let Some(records) = self.pattern_emission_audit.as_mut() {
            records.clear();
        }
        self.pattern_emission_audit_dropped = 0;
        self.view_clipped_patterns = false;
        self.text_labels.clear();
        // World map separate buffers
        self.world_map_line_vertices.clear();
        self.world_map_line_indices.clear();
        self.world_map_mask_vertices.clear();
        self.world_map_mask_indices.clear();

        // During animation (preserve_declutter=true), skip screen-space declutter
        // to prevent symbols from disappearing due to changed screen coordinates
        self.skip_screen_declutter = preserve_declutter;

        // world_dedup must ALWAYS be cleared - it's rebuilt each frame from scratch
        // Only screen-space grids (symbol_grid, sounding_screen_grid) should be preserved
        // during animation to prevent flickering
        self.world_dedup.clear();

        if !preserve_declutter {
            self.symbol_grid.clear();
            self.sounding_screen_grid.clear();
            self.sounding_exact_positions.clear();

            // Adjust grid cell size based on zoom level
            // At low zoom (zoomed out), use larger cells to declutter more aggressively
            // At high zoom (zoomed in), use smaller cells to show more detail
            self.grid_cell_size = (40.0 / self.zoom_level.sqrt() as f32).clamp(20.0, 120.0);

            // Sounding cell size is fixed in screen pixels for consistent density
            // At maximum zoom (>= 45x), show ALL soundings (no filtering)
            if self.zoom_level >= 45.0 {
                self.sounding_cell_size_px = 0.0; // No filtering
            } else {
                // Fixed screen-space cell size for consistent visual density
                // 150px provides good spacing between soundings at most zoom levels
                self.sounding_cell_size_px = 150.0;
            }
        }
    }

    /// Set current zoom level for symbol filtering
    #[inline]
    pub fn set_zoom_level(&mut self, zoom: f64) {
        self.zoom_level = zoom;
    }

    /// Set chart compilation scale (e.g., 22000 for 1:22000)
    #[inline]
    pub fn set_compilation_scale(&mut self, scale: u32) {
        self.compilation_scale = scale;
    }

    /// Calculate the current viewing scale based on zoom level
    /// viewing_scale = compilation_scale / zoom_level
    /// Example: At zoom 2.0 with 1:22000 chart -> viewing scale is 1:11000
    #[inline]
    pub fn viewing_scale(&self) -> u32 {
        self.display_scale
    }

    /// Set Natural Earth world map coastlines for background rendering.
    /// Each inner Vec is a line string: list of [longitude, latitude] pairs.
    pub fn set_world_map(&mut self, coastlines: Vec<Vec<[f64; 2]>>) {
        tracing::info!("World map loaded: {} coastline segments", coastlines.len());
        self.world_map_coastlines = ferrite_render::BackgroundCoastlines::new(coastlines)
            .unwrap_or_else(|e| {
                tracing::warn!("{e}");
                Default::default()
            });
    }

    /// More detailed reference coastlines for regional views; never used as ENC data.
    pub fn set_world_map_detailed(&mut self, coastlines: Vec<Vec<[f64; 2]>>) {
        self.world_map_detailed = ferrite_render::BackgroundCoastlines::new(coastlines)
            .unwrap_or_else(|e| {
                tracing::warn!("{e}");
                Default::default()
            });
    }

    /// Set chart coverage bounding boxes so world map is masked
    /// where chart data exists (opaque background rectangles).
    pub fn set_world_map_chart_boxes(&mut self, boxes: Vec<(f64, f64, f64, f64)>) {
        self.world_map_chart_boxes = boxes;
    }

    /// Set the screen pixel width of 360° longitude for wrapping.
    /// Call this after scaler is configured: `renderer.set_lon_wrap_pixels(360.0 * scaler.scale_x as f32)`
    pub fn set_lon_wrap_pixels(&mut self, px: f32) {
        self.lon_wrap_screen_px = px;
        // Update left/right view uniform buffers immediately so wrapping draws
        // use the correct offset from the very first frame after chart load.
        self.update_view_uniforms();
    }

    /// Add world map coastline lines and chart-coverage mask rectangles.
    /// Uses separate buffers (not chart line_vertices) so they render
    /// independently in the correct draw order:
    ///   1. World map coastlines (lowest layer)
    ///   2. Opaque background rectangles over chart bboxes (mask coastlines)
    ///   3. Chart data on top (priority-based rendering)
    ///
    /// Renders at lon offsets -360°, 0°, +360° for seamless wrapping.
    pub fn add_world_map_lines(&mut self, scaler: &ferrite_render::Scaler) {
        if self.world_map_coastlines.is_empty() && self.world_map_detailed.is_empty() {
            return;
        }

        // Subtle gray color for background coastlines
        let color: [f32; 4] = [0.65, 0.65, 0.65, 1.0];
        let width: f32 = 1.0;

        let vw = scaler.viewport.width;
        let vh = scaler.viewport.height;
        let margin: f32 = 100.0;
        let clip_x_min = scaler.viewport.x - margin;
        let clip_y_min = scaler.viewport.y - margin;
        let clip_x_max = scaler.viewport.x + vw + margin;
        let clip_y_max = scaler.viewport.y + vh + margin;

        // Use actual physical viewport corners, including its origin and clip guard.
        let a = scaler.screen_to_world(ScreenPoint {
            x: clip_x_min,
            y: clip_y_min,
        });
        let b = scaler.screen_to_world(ScreenPoint {
            x: clip_x_max,
            y: clip_y_max,
        });
        let view = [a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y)];
        let detailed = ferrite_render::BackgroundCoastlines::use_detailed(scaler.scale_x())
            && !self.world_map_detailed.is_empty();
        let coastlines = if detailed {
            &self.world_map_detailed
        } else {
            &self.world_map_coastlines
        };
        let lon_offsets: [f64; 3] = [-360.0, 0.0, 360.0];
        for &lon_offset in &lon_offsets {
            for chunk in coastlines.visible_chunks(view, lon_offset) {
                let coastline = &chunk.points;
                let first_lon = coastline[0][0] + lon_offset;
                let mut prev = scaler.world_to_screen(WorldPoint::new(first_lon, coastline[0][1]));

                for pt in &coastline[1..] {
                    let cur_lon = pt[0] + lon_offset;
                    let cur_lat = pt[1];
                    let curr = scaler.world_to_screen(WorldPoint::new(cur_lon, cur_lat));

                    if !prev.x.is_finite()
                        || !prev.y.is_finite()
                        || !curr.x.is_finite()
                        || !curr.y.is_finite()
                    {
                        prev = curr;
                        continue;
                    }

                    if let Some((cx0, cy0, cx1, cy1)) = Self::clip_line_segment(
                        prev.x, prev.y, curr.x, curr.y, clip_x_min, clip_y_min, clip_x_max,
                        clip_y_max,
                    ) {
                        let dx = cx1 - cx0;
                        let dy = cy1 - cy0;
                        let len = (dx * dx + dy * dy).sqrt();

                        if len >= 0.5 {
                            let nx = -dy / len * width * 0.5;
                            let ny = dx / len * width * 0.5;

                            let base_index = self.world_map_line_vertices.len() as u32;

                            self.world_map_line_vertices
                                .push(LineVertex::new(cx0, cy0, -nx, -ny, color));
                            self.world_map_line_vertices
                                .push(LineVertex::new(cx0, cy0, nx, ny, color));
                            self.world_map_line_vertices
                                .push(LineVertex::new(cx1, cy1, nx, ny, color));
                            self.world_map_line_vertices
                                .push(LineVertex::new(cx1, cy1, -nx, -ny, color));

                            self.world_map_line_indices.push(base_index);
                            self.world_map_line_indices.push(base_index + 1);
                            self.world_map_line_indices.push(base_index + 2);
                            self.world_map_line_indices.push(base_index);
                            self.world_map_line_indices.push(base_index + 2);
                            self.world_map_line_indices.push(base_index + 3);
                        }
                    }

                    prev = curr;
                }
            }

            // Add opaque background rectangles over chart bboxes at this lon offset.
            // These mask world map coastlines under loaded chart areas.
            let bg = self.background_color.to_array();
            for &(min_x, min_y, max_x, max_y) in &self.world_map_chart_boxes {
                let shifted_min_x = min_x + lon_offset;
                let shifted_max_x = max_x + lon_offset;

                // Frustum cull
                if !self.is_aabb_visible(shifted_min_x, min_y, shifted_max_x, max_y) {
                    continue;
                }

                let tl = scaler.world_to_screen(WorldPoint::new(shifted_min_x, max_y));
                let br = scaler.world_to_screen(WorldPoint::new(shifted_max_x, min_y));

                if !tl.x.is_finite() || !tl.y.is_finite() || !br.x.is_finite() || !br.y.is_finite()
                {
                    continue;
                }

                let base = self.world_map_mask_vertices.len() as u32;
                self.world_map_mask_vertices
                    .push(Vertex2D::new(tl.x, tl.y, bg));
                self.world_map_mask_vertices
                    .push(Vertex2D::new(br.x, tl.y, bg));
                self.world_map_mask_vertices
                    .push(Vertex2D::new(br.x, br.y, bg));
                self.world_map_mask_vertices
                    .push(Vertex2D::new(tl.x, br.y, bg));

                self.world_map_mask_indices.push(base);
                self.world_map_mask_indices.push(base + 1);
                self.world_map_mask_indices.push(base + 2);
                self.world_map_mask_indices.push(base);
                self.world_map_mask_indices.push(base + 2);
                self.world_map_mask_indices.push(base + 3);
            }
        }
    }

    /// Add drawing instructions from render context
    pub fn add_instructions(&mut self, context: &mut RenderContext) {
        self.add_instructions_with_symbols(context, None, None, None);
    }

    /// Add drawing instructions with symbol rendering support
    /// Uses S-101 compliant priority grouping for correct render order
    ///
    /// # Arguments
    /// * `context` - The render context containing drawing instructions
    /// * `symbol_cache` - Optional symbol cache for SVG rendering
    /// * `color_profile` - Optional color profile for symbol coloring
    /// * `visible_viewing_groups` - Optional set of viewing group IDs that should be visible.
    ///   If None, all viewing groups are visible. Used for Display Mode filtering.
    ///
    /// Retained static overlap graph payload; None means the bounded spatial
    /// planner fallback is active. This is not total renderer memory usage.
    pub fn flat_line_suppression_cache_bytes(&self) -> Option<usize> {
        self.line_suppression.immutable_preparation_bytes()
    }
    pub fn dependency_render_status(&self) -> &DependencyRenderStatus {
        &self.dependency_status
    }

    pub fn add_instructions_with_symbols(
        &mut self,
        context: &mut RenderContext,
        mut symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
    ) {
        context.get_sorted_instructions();
        let flat_dependency_span = self.flat_span(
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::DependencyResolution,
        );
        let graph = context.dependency_graph();
        if !graph.has_parents() {
            drop(flat_dependency_span);
            self.dependency_status = DependencyRenderStatus {
                converged: true,
                ..Default::default()
            };
            self.emit_instructions_with_symbols(
                context,
                symbol_cache,
                color_profile,
                visible_viewing_groups,
                None,
                None,
            );
            return;
        }

        // Retain a frame's pre-existing overlay prefix. Cached textures and
        // triangulation remain reusable; trial geometry is never submitted.
        let lengths = (
            self.area_vertices.len(),
            self.area_indices.len(),
            self.line_vertices.len(),
            self.line_indices.len(),
            self.symbol_instances.len(),
            self.pattern_vertices.len(),
            self.pattern_indices.len(),
            self.text_labels.len(),
            self.area_priority_ranges.len(),
            self.line_priority_ranges.len(),
            self.symbol_priority_ranges.len(),
            self.pattern_ranges.len(),
            self.displayed_geometry.len(),
        );
        let audit_prefix = (
            self.pattern_emission_audit.as_ref().map_or(0, Vec::len),
            self.pattern_emission_audit_dropped,
        );
        let grids = (
            self.symbol_grid.clone(),
            self.sounding_screen_grid.clone(),
            self.sounding_exact_positions.clone(),
            self.world_dedup.clone(),
        );
        let seed = graph
            .resolve(&vec![true; graph.len()])
            .expect("matching dependency graph");
        let mut permission = seed.executed;
        let mut executed = vec![false; graph.len()];
        let mut previous: Option<Vec<bool>> = None;
        self.dependency_status = DependencyRenderStatus {
            missing_parent_count: seed.missing_parent_count,
            ..Default::default()
        };
        // Suppression and collision can feed back into parent execution. Bound
        // trial work and diagnose inconsistent lists rather than hang the UI.
        for pass in 0..65 {
            self.area_vertices.truncate(lengths.0);
            self.area_indices.truncate(lengths.1);
            self.line_vertices.truncate(lengths.2);
            self.line_indices.truncate(lengths.3);
            self.symbol_instances.truncate(lengths.4);
            self.pattern_vertices.truncate(lengths.5);
            self.pattern_indices.truncate(lengths.6);
            self.text_labels.truncate(lengths.7);
            self.area_priority_ranges.truncate(lengths.8);
            self.line_priority_ranges.truncate(lengths.9);
            self.symbol_priority_ranges.truncate(lengths.10);
            self.pattern_ranges.truncate(lengths.11);
            if let Some(records) = self.pattern_emission_audit.as_mut() {
                records.truncate(audit_prefix.0);
            }
            self.pattern_emission_audit_dropped = audit_prefix.1;
            self.displayed_geometry.truncate(lengths.12);
            self.symbol_grid.clone_from(&grids.0);
            self.sounding_screen_grid.clone_from(&grids.1);
            self.sounding_exact_positions.clone_from(&grids.2);
            self.world_dedup.clone_from(&grids.3);
            self.cached_symbol_buffers.clear();
            self.gpu_buffers_dirty = true;
            executed.fill(false);
            self.emit_instructions_with_symbols(
                context,
                symbol_cache.as_deref_mut(),
                color_profile,
                visible_viewing_groups,
                Some(&permission),
                Some(&mut executed),
            );
            self.dependency_status.iterations = pass + 1;
            if self.dependency_status.nonconvergent_diagnostic {
                break;
            }
            let grounded = graph.resolve(&executed).expect("matching execution mask");
            let next = graph
                .permitted_by_executed(&grounded.executed)
                .expect("matching execution mask");
            if next == permission {
                self.dependency_status.converged = true;
                break;
            }
            if previous.as_ref() == Some(&next) || pass == 63 {
                tracing::error!("S-100 Parent execution did not converge; dependent commands withheld, roots retained");
                self.dependency_status.nonconvergent_diagnostic = true;
                permission = context
                    .raw_instructions()
                    .iter()
                    .map(|i| i.dependency().is_none_or(|d| d.parent_id.is_none()))
                    .collect();
            } else {
                previous = Some(std::mem::replace(&mut permission, next));
            }
        }
        self.dependency_status.permitted = permission;
        self.dependency_status.executed = executed;
    }

    fn emit_instructions_with_symbols(
        &mut self,
        context: &mut RenderContext,
        mut symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
        dependency_permission: Option<&[bool]>,
        mut execution: Option<&mut [bool]>,
    ) {
        self.set_raster_enabled_groups(visible_viewing_groups);
        self.chart_geometry_viewport = Some(context.scaler.viewport);
        self.bind_triangulation_context(context);
        let profiling = crate::profiler::is_profiling_enabled();
        let total_timer = if profiling {
            Some(ScopeTimer::new("add_instructions_total"))
        } else {
            None
        };

        // Set animation mode and sort instructions, then extract what we need
        context.set_animation_mode(self.animation_mode);
        // get_sorted_instructions() sorts in-place on first call, then returns &slice
        // Clone scaler (cheap: a few f64 fields) to avoid borrow conflict with &mut self methods
        context
            .scaler
            .set_pixel_ratio(self.state.window.scale_factor());
        let scaler = context.scaler.clone();
        self.geometry_transform = Some(Self::scaler_transform(&scaler));
        self.display_scale = scaler.display_scale.round().clamp(1.0, u32::MAX as f64) as u32;
        context.get_sorted_instructions();
        let flat_visibility_span =
            self.flat_span(ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Visibility);
        let (temporal_visible, hidden, diagnostics) = match context.portrayal_visibility() {
            Ok(visibility) => visibility,
            Err(error) => {
                self.coverage_failed = true;
                tracing::error!("Coverage execution visibility rejected: {error}");
                return;
            }
        };
        drop(flat_visibility_span);
        if let Some(cell) = &self.flat_diagnostic {
            let mut row = cell.borrow_mut();
            row.work.source_commands = row
                .work
                .source_commands
                .saturating_add(context.raw_instructions().len() as u64);
        }
        if diagnostics > 0 && self.temporal_visibility_counts.1 != diagnostics {
            tracing::warn!("Temporal selector: {diagnostics} primitives preserved due to unsupported or invalid time conditions");
        }
        self.temporal_visibility_counts = (hidden, diagnostics);
        self.device_fixed_sources = context
            .raw_instructions()
            .iter()
            .enumerate()
            .filter_map(|(i, command)| command.portrayal_origin().is_device_fixed().then_some(i))
            .collect();
        let flat_gpu_coverage_start = self
            .flat_diagnostic
            .as_ref()
            .map(|_| std::time::Instant::now());
        self.prepared_coverage = match context.prepared_coverage_binding() {
            Ok(binding) => binding,
            Err(error) => {
                self.coverage_failed = true;
                tracing::error!("Coverage binding rejected: {error}");
                return;
            }
        };
        self.emitting_coverage_source = None;
        self.coverage_frame = None;
        self.coverage_failed = false;
        if let Some(prepared) = &self.prepared_coverage {
            let result = (|| -> Result<crate::coverage_gpu_frame::CoverageGpuFrame> {
                let plan = crate::coverage_gpu_frame::CoverageGpuPlan::new_with_frame_local_reuse(
                    prepared,
                    context.geometry_revision(),
                    context.coverage_view_revision(),
                    context.instruction_count(),
                    if self.lon_wrap_screen_px > 0. { 3 } else { 1 },
                    self.state.device.limits().max_texture_dimension_2d,
                    128 * 1024 * 1024,
                    self.frame_local_coverage_clip_reuse,
                )?;
                if plan.mask_count() > 0 && self.coverage_pipelines.is_none() {
                    self.coverage_pipelines = Some(
                        crate::coverage_pipeline::CoveragePipelines::new_with_symbol_instancing(
                            &self.state.device,
                            self.state.format(),
                            crate::state::MSAA_SAMPLE_COUNT,
                            &self.pipelines.view_bind_group_layout,
                            &self.pipelines.texture_bind_group_layout,
                            &self.pipelines.pattern_bind_group_layout,
                            self.pipelines.symbol_instance_pipeline.is_some(),
                        )?,
                    );
                }
                let fallback;
                let layout = if let Some(pipelines) = &self.coverage_pipelines {
                    &pipelines.clip_layout
                } else {
                    fallback = crate::coverage_clip::create_clip_layout(&self.state.device);
                    &fallback
                };
                crate::coverage_gpu_frame::CoverageGpuFrame::upload(
                    &self.state.device,
                    &self.state.queue,
                    layout,
                    plan,
                )
            })();
            match result {
                Ok(frame) => self.coverage_frame = Some(frame),
                Err(error) => {
                    self.coverage_failed = true;
                    tracing::error!("Coverage upload rejected: {error}");
                    return;
                }
            }
        }
        self.update_coverage_transform();
        if let Some(start) = flat_gpu_coverage_start {
            let elapsed = start.elapsed();
            self.flat_gpu_coverage_host_ns = self
                .flat_gpu_coverage_host_ns
                .saturating_add(elapsed.as_nanos().min(u64::MAX as u128) as u64);
            self.record_flat_stage(
                ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Coverage,
                elapsed,
            );
        }
        let instructions = context.raw_instructions();
        let visibility = dependency_permission.map(|permission| {
            temporal_visible
                .iter()
                .zip(permission)
                .map(|(date, parent)| *date && *parent)
                .collect::<Vec<_>>()
        });
        let execution_visibility = visibility.as_deref().unwrap_or(&temporal_visible);

        self.view_dependent_symbols = instructions.iter().any(|p| {
            p.portrayal_origin().requires_view_reprojection()
                || match p {
                    DrawingInstruction::Point(p) => {
                        p.line_placement.is_some()
                            || p.rotation_crs == ferrite_render::RotationCrs::Geographic
                            || p.curve_tangent_bearing.is_some()
                    }
                    DrawingInstruction::Text(t) => {
                        t.rotation_crs == ferrite_render::RotationCrs::Geographic
                            || t.curve_tangent_bearing.is_some()
                    }
                    _ => false,
                }
        });
        let _instruction_count = instructions.len();
        let mut text_instruction_indices = execution
            .as_ref()
            .map(|_| vec![None; self.text_labels.len()]);

        // Update viewport bounds for frustum culling
        self.update_viewport_bounds(&scaler);

        // =====================================================================
        // S-100 Part 9-11.1.9: Line suppression pre-pass
        // =====================================================================
        // When multiple features share the same curve geometry, only the
        // highest-priority LineInstruction is rendered. Lines marked as
        // unsuppressible (LineInstructionUnsuppressed) always render.
        //
        // Build a map from curve geometry hash -> highest priority that claims it.
        // A curve is identified by hashing all its world-coordinate points, so two
        // line instructions referencing the same spatial curve produce the same key.
        // =====================================================================
        // Line suppression: use cached set if instructions haven't changed
        // =====================================================================
        let suppression_timer = if profiling {
            Some(ScopeTimer::new("line_suppression"))
        } else {
            None
        };
        let suppressed_lines = self
            .line_suppression
            .plan_context_projected_with_visibility(
                context,
                self.viewing_scale(),
                visible_viewing_groups,
                self.show_soundings.then_some(33010),
                Some(execution_visibility),
            );
        if let Some(t) = suppression_timer {
            self.cpu_profiler.record("line_suppression", t.elapsed());
        }

        // Pre-compute world→screen transform once for all areas
        let area_transform = Self::scaler_transform(&scaler);

        // Track skipped counts for debugging
        let mut _culled_count = 0usize;

        // Per-type timing accumulators
        let mut area_time = std::time::Duration::ZERO;
        let mut line_time = std::time::Duration::ZERO;
        let mut symbol_time = std::time::Duration::ZERO;
        let mut text_time = std::time::Duration::ZERO;
        let mut area_count = 0u32;
        let mut line_count = 0u32;
        let mut symbol_count = 0u32;
        let mut text_count = 0u32;

        // S-101 Priority tracking: track index ranges per (display_plane, priority)
        let mut current_priority: Option<i32> = None;
        let mut current_coverage_source: Option<usize> = None;
        let mut current_plane =
            ferrite_render::DisplayPlane::UnderRadar.composition_plane(CompositionStage::Chart);
        let mut area_start_idx = 0usize;
        let mut line_start_idx = 0usize;
        let mut symbol_start_idx = 0usize;

        // Pre-compute viewing scale once (constant during entire instruction loop)
        let viewing_scale = self.viewing_scale();

        // Retained hierarchy rejects groups of offscreen areas before geometry
        // key hashing, pattern/hatch preparation or ring scans. Original order,
        // suppression and exact culling inside the emitting routines are retained.
        let mut area_candidates = vec![true; instructions.len()];
        if self.spatial_hierarchy_enabled {
            let index = context.scene_spatial_index();
            for id in index.areas.ids() {
                area_candidates[id] = false;
            }
            index.areas.query_classified(
                |b| {
                    longitude_bounds_relation(
                        (b.min[0], b.min[1], b.max[0], b.max[1]),
                        self.viewport_world_bounds,
                        self.lon_wrap_screen_px > 0.,
                    )
                },
                |id| area_candidates[id] = true,
            );
        }

        // Select sounding champions BEFORE emitting any texture instances. A later
        // shallower sounding must not leave the earlier deeper digits on screen.
        // Tie-breaking by geographic position makes results independent of HashMap order.
        if !self.skip_screen_declutter && self.sounding_cell_size_px > 0.1 {
            let cell_px = self.sounding_cell_size_px as f64 * self.state.window.scale_factor();
            let cell_x = cell_px / scaler.scale_x().abs();
            let cell_y = cell_px / scaler.scale_y().abs();
            if cell_x.is_finite() && cell_y.is_finite() && cell_x > 0.0 && cell_y > 0.0 {
                for (index, instruction) in instructions.iter().enumerate() {
                    if !execution_visibility[index] {
                        continue;
                    }
                    let DrawingInstruction::Point(point) = instruction else {
                        continue;
                    };
                    if point.portrayal_origin.is_device_fixed()
                        || !point.scale_range.is_visible_at(viewing_scale)
                        || !self.is_point_visible(point.position.x, point.position.y)
                        || !self.show_soundings
                    {
                        continue;
                    }
                    if let Some(visible) = visible_viewing_groups {
                        if !instruction
                            .viewing_groups()
                            .all(|g| visible.contains(&g.0) || g.0 == 33010)
                        {
                            continue;
                        }
                    }
                    let id = intern_symbol(&point.symbol_ref);
                    if self.get_symbol_flags(id, &point.symbol_ref) & SYM_SOUNDING == 0 {
                        continue;
                    }
                    let key = (
                        (point.position.x / cell_x).floor() as i32,
                        (scaler.projection().project_y(point.position.y) / cell_y).floor() as i32,
                    );
                    let exact = (
                        (point.position.x * 1_000_000.0) as i64,
                        (point.position.y * 1_000_000.0) as i64,
                    );
                    let depth = point.depth().unwrap_or(f64::MAX);
                    let candidate = (exact, depth);
                    let entry = self.sounding_screen_grid.entry(key).or_insert(candidate);
                    if depth.total_cmp(&entry.1).is_lt() || (depth == entry.1 && exact < entry.0) {
                        *entry = candidate;
                    }
                }
                self.sounding_exact_positions.extend(
                    self.sounding_screen_grid
                        .values()
                        .map(|(position, _)| *position),
                );
            }
        }

        for (inst_idx, instruction) in instructions.iter().enumerate() {
            let _flat_instruction_span = if self.flat_diagnostic.is_some() {
                self.flat_span(match instruction {
                    DrawingInstruction::Area(_) => {
                        ferrite_render::flat_reuse_diagnostics::FlatFrameStage::AreaAndPattern
                    }
                    DrawingInstruction::Line(_) => {
                        ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Lines
                    }
                    DrawingInstruction::Point(_) => {
                        ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Points
                    }
                    DrawingInstruction::Text(_) => {
                        ferrite_render::flat_reuse_diagnostics::FlatFrameStage::TextAndDeclutter
                    }
                })
            } else {
                None
            };
            if !area_candidates[inst_idx] || !execution_visibility[inst_idx] {
                continue;
            }
            if !ferrite_render::instruction_visible(
                instruction,
                viewing_scale,
                visible_viewing_groups,
                self.show_soundings.then_some(33010),
            ) {
                continue;
            }

            if let Some(cell) = &self.flat_diagnostic {
                let mut row = cell.borrow_mut();
                row.work.executed_commands = row.work.executed_commands.saturating_add(1);
            }
            let inst_priority = instruction.priority().0;
            let inst_plane = instruction
                .display_plane()
                .composition_plane(CompositionStage::Chart);

            let instruction_source = (self.prepared_coverage.is_some()
                || self.device_fixed_sources.contains(&inst_idx))
            .then_some(inst_idx);
            let same_coverage = (self.device_fixed_sources.contains(&inst_idx)
                == current_coverage_source.is_some_and(|s| self.device_fixed_sources.contains(&s)))
                && match (&self.prepared_coverage, current_coverage_source) {
                    (Some(binding), Some(previous)) => binding
                        .same_draw_decisions(previous, inst_idx)
                        .unwrap_or(false),
                    (None, _) => current_coverage_source.is_none() && instruction_source.is_none(),
                    _ => false,
                };
            // Check if priority, display plane or coverage decisions changed.
            if let Some(prev_priority) = current_priority {
                if prev_priority != inst_priority || current_plane != inst_plane || !same_coverage {
                    // Record area range if any areas were added for previous group
                    if self.area_indices.len() > area_start_idx {
                        self.area_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            area_start_idx,
                            self.area_indices.len(),
                            current_coverage_source,
                        ));
                    }
                    area_start_idx = self.area_indices.len();

                    // Record line range if any lines were added for previous group
                    if self.line_indices.len() > line_start_idx {
                        self.line_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            line_start_idx,
                            self.line_indices.len(),
                            current_coverage_source,
                        ));
                    }
                    line_start_idx = self.line_indices.len();

                    // Record symbol range if any symbols were added for previous group
                    if self.symbol_instances.len() > symbol_start_idx {
                        self.symbol_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            symbol_start_idx,
                            self.symbol_instances.len(),
                            current_coverage_source,
                        ));
                    }
                    symbol_start_idx = self.symbol_instances.len();
                }
            }
            current_priority = Some(inst_priority);
            current_plane = inst_plane;
            current_coverage_source = instruction_source;
            self.emitting_coverage_source = instruction_source;

            // S-100 Scale-dependent visibility: skip instructions outside their scale range
            if !instruction.scale_range().is_visible_at(viewing_scale) {
                continue;
            }

            let inst_start = if profiling {
                Some(std::time::Instant::now())
            } else {
                None
            };

            let before_execution = execution.as_ref().map(|_| {
                (
                    self.area_indices.len(),
                    self.line_indices.len(),
                    self.pattern_indices.len(),
                    self.symbol_instances.len(),
                    self.text_labels.len(),
                )
            });
            match instruction {
                DrawingInstruction::Area(area) => {
                    let before = (
                        self.area_indices.len(),
                        self.pattern_indices.len(),
                        self.symbol_instances.len(),
                    );
                    // Pattern fills: tile symbols inside the polygon area
                    if let ferrite_render::AreaFillType::Pattern {
                        ref symbol_ref,
                        v1,
                        v2,
                    } = area.fill
                    {
                        // Only the validated independent shallow selector may hide this pattern.
                        if ferrite_render::pattern_display_allows(
                            instruction,
                            self.ui_state.settings.show_shallow_pattern,
                            symbol_cache
                                .as_ref()
                                .and_then(|cache| cache.shallow_pattern_contract()),
                        ) {
                            if let (Some(cache), Some(profile)) =
                                (symbol_cache.as_mut(), color_profile)
                            {
                                self.tile_area_with_pattern(
                                    area,
                                    symbol_ref,
                                    v1,
                                    v2,
                                    &scaler,
                                    cache,
                                    profile,
                                    inst_priority,
                                    inst_idx,
                                );
                            }
                        }
                    } else if let ferrite_render::AreaFillType::HatchFill {
                        color,
                        width,
                        spacing,
                        angle,
                    } = &area.fill
                    {
                        self.tile_area_with_hatch(
                            area,
                            *color,
                            *width,
                            *spacing,
                            *angle,
                            &scaler,
                            inst_priority,
                        );
                    } else {
                        self.add_area_cached(area, area_transform);
                    }
                    if before
                        != (
                            self.area_indices.len(),
                            self.pattern_indices.len(),
                            self.symbol_instances.len(),
                        )
                    {
                        self.displayed_geometry.push(inst_idx);
                    }
                    if let Some(s) = inst_start {
                        area_time += s.elapsed();
                        area_count += 1;
                    }
                }
                DrawingInstruction::Line(line) => {
                    // S-100 Part 9-11.1.9: Skip suppressed lines (lower-priority
                    // suppressible lines on curves already claimed by higher priority)
                    if suppressed_lines.contains(&inst_idx) {
                        _culled_count += 1;
                        continue;
                    }
                    let before = self.line_indices.len();
                    self.add_line(line, &scaler, suppressed_lines.spans(inst_idx));
                    if self.line_indices.len() > before {
                        self.displayed_geometry.push(inst_idx);
                    }
                    if let Some(s) = inst_start {
                        line_time += s.elapsed();
                        line_count += 1;
                    }
                }
                DrawingInstruction::Point(point) => {
                    // Suppress point symbols when zoomed out far beyond chart scale

                    if point.line_placement.is_none()
                        && !point.portrayal_origin.requires_view_reprojection()
                        && !self.is_point_visible(point.position.x, point.position.y)
                    {
                        _culled_count += 1;
                        continue;
                    }
                    let resolved = if point.line_placement.is_some() {
                        match ferrite_render::resolve_flat_line_symbol(point, &scaler) {
                            Ok(p) => p,
                            Err(error) => {
                                tracing::warn!("Line symbol placement: {}", error);
                                continue;
                            }
                        }
                    } else {
                        Vec::new()
                    };
                    let points: &[ferrite_render::PointInstruction] =
                        if point.line_placement.is_some() {
                            &resolved
                        } else {
                            std::slice::from_ref(point)
                        };
                    if points.is_empty() {
                        continue;
                    }
                    // Pre-classify via cached flags (intern is fast: read-lock only)
                    let sym_id = intern_symbol(&point.symbol_ref);
                    let flags = self.get_symbol_flags(sym_id, &point.symbol_ref);
                    let is_sounding = flags & SYM_SOUNDING != 0;

                    // Soundings: respect show_soundings toggle
                    if is_sounding && !self.show_soundings {
                        continue;
                    }

                    // A fully culled point batch has no symbol-render attempt. Do not
                    // diagnose its resource/profile as missing merely because any=false.
                    // Device/reprojected anchors still bypass world-position culling.
                    if !points.iter().any(|point| {
                        point.portrayal_origin.requires_view_reprojection()
                            || self.is_point_visible(point.position.x, point.position.y)
                    }) {
                        continue;
                    }

                    // Try to render the symbol. We require both the cache and an
                    // active color profile — without a profile, SVG color tokens
                    // cannot be resolved and the rendered symbol would be wrong.
                    let rendered = match (symbol_cache.as_mut(), color_profile) {
                        (Some(cache), Some(profile)) => {
                            let mut any = false;
                            for point in points {
                                if point.portrayal_origin.requires_view_reprojection()
                                    || self.is_point_visible(point.position.x, point.position.y)
                                {
                                    any |=
                                        self.try_add_symbol(point, &scaler, cache, profile, sym_id);
                                }
                            }
                            any
                        }
                        _ => false,
                    };

                    // CLAUDE.md: "no placeholder colors, no fallbacks". If the symbol
                    // cannot render, surface it as a real error (logged once per id)
                    // and skip — never paint a hardcoded red square.
                    if !rendered {
                        self.note_unrendered_symbol(sym_id, &point.symbol_ref);
                    }
                    if let Some(s) = inst_start {
                        symbol_time += s.elapsed();
                        symbol_count += 1;
                    }
                }
                DrawingInstruction::Text(text) => {
                    if !text.has_visible_content() {
                        continue;
                    }
                    // Frustum culling: skip text outside viewport
                    if !text.portrayal_origin.requires_view_reprojection()
                        && !self.is_point_visible(text.position.x, text.position.y)
                    {
                        continue;
                    }

                    let rotation = match ferrite_render::flat_text_rotation(text, &scaler) {
                        Ok(value) => value,
                        Err(reason) => {
                            tracing::warn!("Text rotation withheld: {reason}");
                            continue;
                        }
                    };
                    // Convert world position to screen coordinates
                    let screen = match text
                        .portrayal_origin
                        .flat_glyph_anchor(text.position, &scaler)
                    {
                        Ok(p) => p,
                        Err(error) => {
                            tracing::warn!("Text device anchor rejected: {error}");
                            continue;
                        }
                    };

                    // S-100 Part 9a-11.2.2.4: FontSize is in typographic points (pt).
                    // S-101 Lua rules emit values like 10 (= 10pt standard body text).
                    // Convert points → pixels: pts * (DPI / 72), where 1pt = 1/72 inch.
                    let dpi_scale = self.state.window.scale_factor() as f32;
                    let screen_dpi = 96.0 * dpi_scale;
                    let font_size_px = text.font_size * screen_dpi / 72.0;

                    // Apply offset (in mm from Lua LocalOffset, convert to pixels)
                    let offset_x = text.offset.x * SCREEN_PX_PER_MM * dpi_scale;
                    let offset_y = text.offset.y * SCREEN_PX_PER_MM * dpi_scale;
                    let sx = screen.x + offset_x;
                    let sy = screen.y - offset_y;

                    self.text_labels.push(TextLabel {
                        source: self.emitting_coverage_source,
                        plane: inst_plane,
                        priority: inst_priority,
                        anchor: [screen.x, screen.y],
                        rotation,
                        screen_x: sx,
                        screen_y: sy,
                        text: text.text.clone(),
                        font_size: font_size_px,
                        color: [text.color.r, text.color.g, text.color.b, text.color.a],
                        background: text
                            .background
                            .filter(|c| c.a.is_finite() && c.a > 0.)
                            .map(|c| c.to_array()),
                        bold: text.bold,
                        italic: text.italic,
                        h_align: text.h_align,
                        v_align: text.v_align,
                    });
                    if let Some(indices) = &mut text_instruction_indices {
                        indices.push(Some(inst_idx));
                    }
                    if let Some(s) = inst_start {
                        text_time += s.elapsed();
                        text_count += 1;
                    }
                }
            }
            if let (Some(before), Some(mask)) = (before_execution, execution.as_deref_mut()) {
                mask[inst_idx] = before
                    != (
                        self.area_indices.len(),
                        self.line_indices.len(),
                        self.pattern_indices.len(),
                        self.symbol_instances.len(),
                        self.text_labels.len(),
                    );
                if matches!(instruction, DrawingInstruction::Point(_)) {
                    mask[inst_idx] = self.symbol_instances[before.3..]
                        .iter()
                        .any(|instance| self.symbol_quad(instance).is_some());
                }
            }
        }

        if let (Some(indices), Some(mask)) = (text_instruction_indices, execution) {
            if !self.text_labels.is_empty() {
                self.egui.ensure_font_metrics(&self.state.window);
                let (_, accepted) = self.layout_chart_text(Vec::new(), false);
                for (i, source) in indices.into_iter().enumerate() {
                    if let Some(source) = source {
                        mask[source] = accepted[i];
                    }
                }
            }
        }

        // Log per-type instruction timing
        if profiling {
            self.cpu_profiler.record("inst_area", area_time);
            self.cpu_profiler.record("inst_line", line_time);
            self.cpu_profiler.record("inst_symbol", symbol_time);
            self.cpu_profiler.record("inst_text", text_time);
            tracing::debug!(
                "[PROFILER] Instructions: area={} ({:.2}ms), line={} ({:.2}ms), symbol={} ({:.2}ms), text={} ({:.2}ms)",
                area_count, area_time.as_secs_f64() * 1000.0,
                line_count, line_time.as_secs_f64() * 1000.0,
                symbol_count, symbol_time.as_secs_f64() * 1000.0,
                text_count, text_time.as_secs_f64() * 1000.0,
            );
        }

        if let Some(t) = total_timer {
            let elapsed = t.elapsed();
            self.cpu_profiler.record("add_instructions_total", elapsed);
            tracing::debug!(
                "[PROFILER] add_instructions_total: {:.2}ms (areas: {}v/{}i, lines: {}v/{}i, symbols: {}, texts: {})",
                elapsed.as_secs_f64() * 1000.0,
                self.area_vertices.len(), self.area_indices.len(),
                self.line_vertices.len(), self.line_indices.len(),
                self.symbol_instances.len(), self.text_labels.len(),
            );
        }

        // Record final priority ranges
        if let Some(final_priority) = current_priority {
            if self.area_indices.len() > area_start_idx {
                self.area_priority_ranges.push((
                    current_plane,
                    final_priority,
                    area_start_idx,
                    self.area_indices.len(),
                    current_coverage_source,
                ));
            }
            if self.line_indices.len() > line_start_idx {
                self.line_priority_ranges.push((
                    current_plane,
                    final_priority,
                    line_start_idx,
                    self.line_indices.len(),
                    current_coverage_source,
                ));
            }
            if self.symbol_instances.len() > symbol_start_idx {
                self.symbol_priority_ranges.push((
                    current_plane,
                    final_priority,
                    symbol_start_idx,
                    self.symbol_instances.len(),
                    current_coverage_source,
                ));
            }
        }
        // Invalidate intermediate/final picking envelopes without projecting every
        // polygon again. The first actual pick builds only the final draw list.
        self.selection_index.take();
        self.temporal_visibility_mask = temporal_visible;
        self.emitting_coverage_source = None;
    }

    /// O(1) allocation key, valid only within the bound context geometry revision.
    /// Interior rings cannot change through the context's immutable instruction API.
    fn area_geometry_key(
        area: &ferrite_render::AreaInstruction,
        projection: ferrite_render::FlatProjection,
    ) -> (usize, usize, ferrite_render::FlatProjection) {
        (
            area.exterior.as_ptr() as usize,
            area.exterior.len(),
            projection,
        )
    }

    /// Get or compute cached triangulation for an area polygon.
    /// Triangulation is done in world coordinates so it only needs to run once per unique polygon.
    /// Ensure triangulation is cached for this area, returning the cache key.
    /// Returns None if the area cannot be triangulated.
    fn ensure_triangulated(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        projection: ferrite_render::FlatProjection,
    ) -> Option<(usize, usize, ferrite_render::FlatProjection)> {
        let cache_key = Self::area_geometry_key(area, projection);

        // Check cache first
        if self.triangulation_cache.contains_key(&cache_key) {
            if self.area_triangulation_reuse_enabled {
                self.retained_area_triangulations.cache_hits = self
                    .retained_area_triangulations
                    .cache_hits
                    .saturating_add(1);
            }
            if let Some(cell) = &self.flat_diagnostic {
                let mut row = cell.borrow_mut();
                row.work.triangulation_hits = row.work.triangulation_hits.saturating_add(1);
            }
            return Some(cache_key);
        }

        if self.triangulation_failures.contains(&cache_key) {
            return None;
        }
        if let Some(cell) = &self.flat_diagnostic {
            let mut row = cell.borrow_mut();
            row.work.triangulation_cold = row.work.triangulation_cold.saturating_add(1);
        }
        if self.area_triangulation_reuse_enabled {
            self.retained_area_triangulations.cold_evaluations = self
                .retained_area_triangulations
                .cold_evaluations
                .saturating_add(1);
        }
        let triangulated = (|| -> std::result::Result<(Vec<f64>, Vec<usize>), String> {
            let project_ring = |ring: &[WorldPoint]| {
                let points: Vec<_> = ring
                    .iter()
                    .map(|p| [p.x, projection.project_y(p.y)])
                    .collect();
                ferrite_kernel::triangulation::normalize_ring(&points).map_err(|e| e.to_string())
            };
            let mut vertices = project_ring(&area.exterior)?;
            let mut holes = Vec::new();
            for hole in &area.interiors {
                holes.push(vertices.len() / 2);
                vertices.extend(project_ring(hole)?);
            }
            let indices = ferrite_kernel::triangulation::triangulate(&vertices, &holes)
                .map_err(|e| e.to_string())?;
            Ok((vertices, indices))
        })();
        let (vertices, indices) = match triangulated {
            Ok(result) => result,
            Err(error) => {
                self.triangulation_failures.insert(cache_key);
                tracing::warn!("Area fill rejected without exterior fallback: {error}");
                return None;
            }
        };

        // Compute world AABB for frustum culling
        let mut aabb_min_x = f64::MAX;
        let mut aabb_min_y = f64::MAX;
        let mut aabb_max_x = f64::MIN;
        let mut aabb_max_y = f64::MIN;
        let vc = vertices.len() / 2;
        for i in 0..vc {
            let x = vertices[i * 2];
            let y = vertices[i * 2 + 1];
            if x < aabb_min_x {
                aabb_min_x = x;
            }
            if y < aabb_min_y {
                aabb_min_y = y;
            }
            if x > aabb_max_x {
                aabb_max_x = x;
            }
            if y > aabb_max_y {
                aabb_max_y = y;
            }
        }

        let cached = CachedTriangulation {
            indices,
            world_vertices: vertices,
            world_aabb: (
                aabb_min_x,
                projection.unproject_y(aabb_min_y),
                aabb_max_x,
                projection.unproject_y(aabb_max_y),
            ),
        };

        self.triangulation_cache.insert(
            cache_key,
            if self.area_triangulation_reuse_enabled {
                TriangulationStorage::Shared(Arc::new(cached))
            } else {
                TriangulationStorage::Owned(cached)
            },
        );
        Some(cache_key)
    }

    /// Pre-computed world→screen transform parameters (avoids per-area scaler lookups)
    #[inline]
    fn scaler_transform(scaler: &ferrite_render::Scaler) -> ferrite_render::FlatTransform {
        scaler.flat_transform()
    }

    fn add_area_cached(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        transform: ferrite_render::FlatTransform,
    ) {
        // Get fill color — pattern/centroid/hatch fills are overlays, not solid fills
        let color = match &area.fill {
            ferrite_render::AreaFillType::Solid(c) => c.to_array(),
            ferrite_render::AreaFillType::Pattern { .. }
            | ferrite_render::AreaFillType::HatchFill { .. }
            | ferrite_render::AreaFillType::CentroidSymbol(_) => return,
        };

        let cache_key = Self::area_geometry_key(area, transform.projection);

        // Fast path: triangulation already cached — use cached AABB for O(1) frustum culling
        // (avoids re-scanning entire exterior ring just to compute AABB)
        if let Some(cached) = self.triangulation_cache.get(&cache_key) {
            // Frustum culling with cached AABB
            if let Some((vp_min_x, vp_min_y, vp_max_x, vp_max_y)) = self.viewport_world_bounds {
                let (ax, ay, bx, by) = cached.world_aabb;
                let margin_x = (vp_max_x - vp_min_x) * 0.5;
                let margin_y = (vp_max_y - vp_min_y) * 0.5;
                let copies = if self.lon_wrap_screen_px > 0. {
                    &[0., -360., 360.][..]
                } else {
                    &[0.][..]
                };
                let outside_x = copies
                    .iter()
                    .all(|dx| bx + dx < vp_min_x - margin_x || ax + dx > vp_max_x + margin_x);
                if outside_x || by < vp_min_y - margin_y || ay > vp_max_y + margin_y {
                    return;
                }
            }

            let total_vertex_count = cached.world_vertices.len() / 2;
            if let Some(cell) = &self.flat_diagnostic {
                let mut row = cell.borrow_mut();
                row.work.triangulation_hits = row.work.triangulation_hits.saturating_add(1);
                row.work.projected_vertices = row
                    .work
                    .projected_vertices
                    .saturating_add(total_vertex_count as u64);
                row.work.triangles = row
                    .work
                    .triangles
                    .saturating_add((cached.indices.len() / 3) as u64);
            }
            let wv_ptr = cached.world_vertices.as_ptr();
            let wv_len = cached.world_vertices.len();
            let idx_ptr = cached.indices.as_ptr();
            let idx_len = cached.indices.len();
            // SAFETY: triangulation_cache is not modified during the loops below,
            // and these pointers remain valid because we don't mutate the cache.
            let wv = unsafe { std::slice::from_raw_parts(wv_ptr, wv_len) };
            let indices = unsafe { std::slice::from_raw_parts(idx_ptr, idx_len) };

            let base_index = self.area_vertices.len() as u32;
            let [scale_x, scale_y] = transform.scale;
            let [offset_x, offset_y] = transform.offset;
            let [min_x, max_lat] = transform.geographic_origin;
            let max_y = transform.projection.project_y(max_lat);

            self.area_vertices.reserve(total_vertex_count);
            self.area_vertices.extend((0..total_vertex_count).map(|i| {
                let wx = wv[i * 2];
                let wy = wv[i * 2 + 1];
                let sx = ((wx - min_x) * scale_x + offset_x) as f32;
                let sy = ((max_y - wy) * scale_y + offset_y) as f32;
                Vertex2D::new(sx, sy, color)
            }));

            self.area_indices.reserve(idx_len);
            self.area_indices
                .extend(indices.iter().map(|&i| base_index + i as u32));

            return;
        }

        // Cold path: first-time triangulation — fall back to ring-scan culling
        if !Self::is_ring_visible_static(
            &area.exterior,
            self.viewport_world_bounds,
            self.lon_wrap_screen_px > 0.0,
        ) {
            return;
        }

        // Ensure triangulation is cached
        if self
            .ensure_triangulated(area, transform.projection)
            .is_none()
        {
            return;
        }

        // Recurse once: now the cache is populated, fast path will handle it
        self.add_area_cached(area, transform);
    }

    /// Fill an area polygon with a tiled pattern texture (S-100 standard).
    ///
    /// Uses GPU texture repeat mode (like OpenS100's D2D1_EXTEND_MODE_WRAP):
    /// triangulates the polygon and assigns UV coordinates with optional shear
    /// for parallelogram tiling (S-100 Part 9a: v1/v2 lattice vectors).
    #[allow(clippy::too_many_arguments)]
    fn tile_area_with_pattern(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        symbol_ref: &str,
        v1: (f32, f32),
        v2: (f32, f32),
        scaler: &ferrite_render::Scaler,
        symbol_cache: &mut SymbolCache,
        color_profile: &ColorProfile,
        priority: i32,
        source_ordinal: usize,
    ) {
        // Frustum culling: quick AABB check on exterior ring
        if !Self::is_ring_visible_static(
            &area.exterior,
            self.viewport_world_bounds,
            self.lon_wrap_screen_px > 0.0,
        ) {
            return;
        }

        // Apply HiDPI scale factor so pattern matches physical mm on screen
        let dpi_scale = self.state.window.scale_factor() as f32;
        let mm_to_px = SCREEN_PX_PER_MM * dpi_scale;

        // S-100: v1 is the horizontal period, v2 defines the row offset
        // Texture tile size = |v1| width × |v2.y| height (rectangular tile)
        // Parallelogram offset = v2.x (horizontal shift per row)
        let v1_len = (v1.0 * v1.0 + v1.1 * v1.1).sqrt();
        let spacing_x_px = (v1_len * mm_to_px).max(4.0);
        let spacing_y_px = (v2.1.abs() * mm_to_px).max(4.0);
        // Shear ratio: how much each row shifts horizontally (in UV units)
        let shear = if v2.1.abs() > 0.001 { v2.0 / v2.1 } else { 0.0 };

        // Ensure pattern texture exists in GPU cache
        let pat_key = crate::symbol_cache::pattern_texture_key(
            symbol_ref,
            spacing_x_px,
            spacing_y_px,
            mm_to_px,
        );
        if !self.pattern_textures.contains_key(&pat_key) {
            let geom = match symbol_cache.get_symbol_for_pattern(
                symbol_ref,
                color_profile,
                spacing_x_px,
                spacing_y_px,
                mm_to_px,
            ) {
                Some(g) => g,
                None => {
                    tracing::warn!("Pattern fill symbol '{}' not found", symbol_ref);
                    return;
                }
            };
            let tex_w = geom.width;
            let tex_h = geom.height;
            let (texture, view) = self.state.create_texture_from_rgba(
                &geom.pixels,
                tex_w,
                tex_h,
                &format!("pattern_{}", symbol_ref),
            );
            let bind_group = self
                .pipelines
                .create_pattern_bind_group(&self.state.device, &view);
            self.pattern_textures.insert(
                pat_key.clone(),
                PatternTexture {
                    texture,
                    bind_group,
                    width: tex_w,
                    height: tex_h,
                },
            );
        }

        // inv_tile_size: use actual texture pixel dimensions for seamless tiling
        let pat_tex = self.pattern_textures.get(&pat_key).unwrap();
        let inv_tx = 1.0 / pat_tex.width as f32;
        let inv_ty = 1.0 / pat_tex.height as f32;

        // Pattern coverage reuses the checked projected polygon, including
        // every valid hole. Do not drop malformed vertices or holes here.
        let Some(key) = self.ensure_triangulated(area, scaler.projection()) else {
            return;
        };
        let cached = &self.triangulation_cache[&key];
        let transform = scaler.flat_transform();
        let [sx, sy] = transform.scale;
        let [ox, oy] = transform.offset;
        let [min_x, max_lat] = transform.geographic_origin;
        let max_y = transform.projection.project_y(max_lat);
        if let Some(cell) = &self.flat_diagnostic {
            let mut row = cell.borrow_mut();
            row.work.projected_vertices = row
                .work
                .projected_vertices
                .saturating_add((cached.world_vertices.len() / 2) as u64);
            row.work.triangles = row
                .work
                .triangles
                .saturating_add((cached.indices.len() / 3) as u64);
        }
        let coords: Vec<f64> = cached
            .world_vertices
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [(p[0] - min_x) * sx + ox, (max_y - p[1]) * sy + oy])
            .collect();
        let indices = &cached.indices;

        // S-100: parallelogram shear ratio (dimensionless).
        // shear = v2.x / v2.y: for each pixel of Y movement, X shifts by shear pixels.
        // The shader computes: u = (pos.x - shear * pos.y) * inv_tx
        let shear_screen = shear;

        // Keep existing modest-coordinate geometry byte-identical. Large
        // triangles are intersected in f64, never rejected by vertex distance.
        let clipped = coords.iter().any(|v| v.abs() > 16000.);
        let idx_start = self.pattern_indices.len();
        let vertex_start = self.pattern_vertices.len();
        let plane = area
            .display_plane
            .composition_plane(CompositionStage::Chart);
        if clipped {
            self.view_clipped_patterns = true;
            let v = scaler.viewport;
            let rect = [
                v.x as f64 - 2.,
                v.y as f64 - 2.,
                (v.x + v.width) as f64 + 2.,
                (v.y + v.height) as f64 + 2.,
            ];
            let wraps = if self.lon_wrap_screen_px > 0. {
                vec![0., -(360. * sx), 360. * sx]
            } else {
                vec![0.]
            };
            for (wrap_mode, dx) in wraps.into_iter().enumerate() {
                let start = self.pattern_indices.len();
                let clipped_vertex_start = self.pattern_vertices.len();
                for tri in indices.as_chunks::<3>().0 {
                    let points =
                        std::array::from_fn(|i| [coords[2 * tri[i]] + dx, coords[2 * tri[i] + 1]]);
                    let Ok(polygon) = ferrite_render::clip_triangle_to_rect(points, rect) else {
                        continue;
                    };
                    let points = polygon.points();
                    if points.len() < 3 {
                        continue;
                    }
                    let base = self.pattern_vertices.len() as u32;
                    self.pattern_vertices.extend(points.iter().map(|p| {
                        PatternVertex::new(p[0] as f32, p[1] as f32, inv_tx, inv_ty, shear_screen)
                    }));
                    for i in 1..points.len() - 1 {
                        self.pattern_indices.extend_from_slice(&[
                            base,
                            base + i as u32,
                            base + i as u32 + 1,
                        ]);
                    }
                }
                let end = self.pattern_indices.len();
                if let Some(records) = self.pattern_emission_audit.as_mut() {
                    record_pattern_emission(
                        records,
                        &mut self.pattern_emission_audit_dropped,
                        PatternEmissionAudit {
                            source_ordinal,
                            vertex_start: clipped_vertex_start,
                            vertex_end: self.pattern_vertices.len(),
                            index_start: start,
                            index_end: end,
                            wrap_mode: wrap_mode as u8,
                            wrap_dx_screen_bits: dx.to_bits(),
                        },
                    );
                }
                self.pattern_ranges.push((
                    plane,
                    priority,
                    start,
                    end,
                    pat_key.clone(),
                    wrap_mode as u8,
                    self.emitting_coverage_source,
                ));
            }
        } else {
            let base = self.pattern_vertices.len() as u32;
            self.pattern_vertices
                .extend(coords.as_chunks::<2>().0.iter().map(|p| {
                    PatternVertex::new(p[0] as f32, p[1] as f32, inv_tx, inv_ty, shear_screen)
                }));
            self.pattern_indices
                .extend(indices.iter().map(|i| base + *i as u32));
        }
        let idx_end = self.pattern_indices.len();
        let plane = area
            .display_plane
            .composition_plane(CompositionStage::Chart);
        if !clipped {
            if let Some(records) = self.pattern_emission_audit.as_mut() {
                record_pattern_emission(
                    records,
                    &mut self.pattern_emission_audit_dropped,
                    PatternEmissionAudit {
                        source_ordinal,
                        vertex_start,
                        vertex_end: self.pattern_vertices.len(),
                        index_start: idx_start,
                        index_end: idx_end,
                        wrap_mode: 255,
                        wrap_dx_screen_bits: 0_f64.to_bits(),
                    },
                );
            }
            self.pattern_ranges.push((
                plane,
                priority,
                idx_start,
                idx_end,
                pat_key,
                255,
                self.emitting_coverage_source,
            ));
        }
    }

    /// S-100 Part 9a hatch fill: render parallel lines inside a polygon area.
    /// Lines are drawn at the specified angle, spacing, and width within the
    /// polygon boundary using line-polygon clipping.
    #[allow(clippy::too_many_arguments)]
    fn tile_area_with_hatch(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        color: Color,
        width: f32,
        spacing_mm: f32,
        angle_deg: f32,
        scaler: &ferrite_render::Scaler,
        _priority: i32,
    ) {
        // Frustum culling: quick AABB check on exterior ring
        if !Self::is_ring_visible_static(
            &area.exterior,
            self.viewport_world_bounds,
            self.lon_wrap_screen_px > 0.0,
        ) {
            return;
        }

        let dpi_scale = self.state.window.scale_factor() as f32;
        let spacing_px = (spacing_mm * SCREEN_PX_PER_MM * dpi_scale).max(2.0);
        let line_width = (width * SCREEN_PX_PER_MM * dpi_scale).max(0.5);
        let color_arr = color.to_array();

        // Convert polygon exterior to screen coordinates (no clamping — clip_line_to_polygon handles bounds)
        let screen_ring: Vec<(f32, f32)> = area
            .exterior
            .iter()
            .map(|p| {
                let s = scaler.world_to_screen(*p);
                (s.x, s.y)
            })
            .filter(|(x, y)| x.is_finite() && y.is_finite())
            .collect();

        if screen_ring.len() < 3 {
            return;
        }

        // Compute bounding box
        let (min_x, min_y, max_x, max_y) = screen_ring.iter().fold(
            (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
            |(mn_x, mn_y, mx_x, mx_y), &(x, y)| {
                (mn_x.min(x), mn_y.min(y), mx_x.max(x), mx_y.max(y))
            },
        );

        // Angle in radians (S-100: 0 = horizontal, CCW positive)
        let angle_rad = angle_deg.to_radians();
        let cos_a = angle_rad.cos();
        let sin_a = angle_rad.sin();

        // Direction perpendicular to the hatch lines (used for spacing)
        let perp_x = -sin_a;
        let perp_y = cos_a;

        // Project bounding box corners onto the perpendicular axis to find range
        let corners = [
            (min_x, min_y),
            (max_x, min_y),
            (max_x, max_y),
            (min_x, max_y),
        ];
        let mut proj_min = f32::MAX;
        let mut proj_max = f32::MIN;
        for &(cx, cy) in &corners {
            let proj = cx * perp_x + cy * perp_y;
            if proj < proj_min {
                proj_min = proj;
            }
            if proj > proj_max {
                proj_max = proj;
            }
        }

        // Diagonal length for extending lines across the entire bounding box
        let diag = ((max_x - min_x).powi(2) + (max_y - min_y).powi(2)).sqrt();

        // Pre-compute viewport bounds for segment culling (hoisted out of loop)
        let hatch_margin = line_width * 2.0 + 50.0;
        let (vp_w, vp_h) = self.state.viewport_size();
        let hatch_clip_min_x = -hatch_margin;
        let hatch_clip_min_y = -hatch_margin;
        let hatch_clip_max_x = vp_w + hatch_margin;
        let hatch_clip_max_y = vp_h + hatch_margin;
        let half_line_width = line_width * 0.5;

        // Generate hatch lines at regular spacing
        let mut d = proj_min;
        while d <= proj_max {
            // Line center point on the perpendicular axis
            let cx = perp_x * d;
            let cy = perp_y * d;

            // Line endpoints extending in the hatch direction across the bbox
            let lx0 = cx - cos_a * diag;
            let ly0 = cy - sin_a * diag;
            let lx1 = cx + cos_a * diag;
            let ly1 = cy + sin_a * diag;

            // Clip this line segment to the polygon using intersection tests
            let segments = Self::clip_line_to_polygon(lx0, ly0, lx1, ly1, &screen_ring);
            for (sx, sy, ex, ey) in segments {
                // Reject hatch segments outside viewport + margin
                if sx < hatch_clip_min_x
                    || sx > hatch_clip_max_x
                    || sy < hatch_clip_min_y
                    || sy > hatch_clip_max_y
                    || ex < hatch_clip_min_x
                    || ex > hatch_clip_max_x
                    || ey < hatch_clip_min_y
                    || ey > hatch_clip_max_y
                {
                    continue;
                }
                // Render as a line quad
                let ldx = ex - sx;
                let ldy = ey - sy;
                let len = (ldx * ldx + ldy * ldy).sqrt();
                if len < 0.001 {
                    continue;
                }
                let nx = -ldy / len * half_line_width;
                let ny = ldx / len * half_line_width;

                let base_index = self.line_vertices.len() as u32;
                self.line_vertices
                    .push(LineVertex::new(sx, sy, -nx, -ny, color_arr));
                self.line_vertices
                    .push(LineVertex::new(sx, sy, nx, ny, color_arr));
                self.line_vertices
                    .push(LineVertex::new(ex, ey, nx, ny, color_arr));
                self.line_vertices
                    .push(LineVertex::new(ex, ey, -nx, -ny, color_arr));

                self.line_indices.push(base_index);
                self.line_indices.push(base_index + 1);
                self.line_indices.push(base_index + 2);
                self.line_indices.push(base_index);
                self.line_indices.push(base_index + 2);
                self.line_indices.push(base_index + 3);
            }

            d += spacing_px;
        }
    }

    /// Clip a line segment to a polygon, returning visible sub-segments.
    /// Uses scanline intersection: find all intersection points of the line
    /// with polygon edges, sort them along the line, then emit inside segments.
    fn clip_line_to_polygon(
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        ring: &[(f32, f32)],
    ) -> Vec<(f32, f32, f32, f32)> {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let line_len_sq = dx * dx + dy * dy;
        if line_len_sq < 1e-10 {
            return Vec::new();
        }

        // Find parametric t values where line intersects each polygon edge
        let mut t_values: Vec<f32> = Vec::with_capacity(8);
        let n = ring.len();
        for i in 0..n {
            let j = (i + 1) % n;
            let (ex0, ey0) = ring[i];
            let (ex1, ey1) = ring[j];

            let edx = ex1 - ex0;
            let edy = ey1 - ey0;

            let denom = dx * edy - dy * edx;
            if denom.abs() < 1e-10 {
                continue; // Parallel
            }

            let t = ((ex0 - x0) * edy - (ey0 - y0) * edx) / denom;
            let u = ((ex0 - x0) * dy - (ey0 - y0) * dx) / denom;

            if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&t) {
                t_values.push(t);
            }
        }

        if t_values.is_empty() {
            // Line might be entirely inside or outside
            let mid_x = (x0 + x1) * 0.5;
            let mid_y = (y0 + y1) * 0.5;
            if point_in_ring(mid_x, mid_y, ring) {
                return vec![(x0, y0, x1, y1)];
            }
            return Vec::new();
        }

        t_values.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Remove near-duplicates
        t_values.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

        // Emit segments between consecutive intersection pairs that are inside
        let mut segments = Vec::with_capacity(t_values.len() / 2 + 1);
        let start_inside = point_in_ring(x0, y0, ring);

        let mut prev_t = 0.0_f32;
        let mut inside = start_inside;

        for &t in &t_values {
            if inside {
                let seg_x0 = x0 + prev_t * dx;
                let seg_y0 = y0 + prev_t * dy;
                let seg_x1 = x0 + t * dx;
                let seg_y1 = y0 + t * dy;
                segments.push((seg_x0, seg_y0, seg_x1, seg_y1));
            }
            inside = !inside;
            prev_t = t;
        }

        // Handle remaining segment to end
        if inside {
            let seg_x0 = x0 + prev_t * dx;
            let seg_y0 = y0 + prev_t * dy;
            segments.push((seg_x0, seg_y0, x1, y1));
        }

        segments
    }

    /// Cohen-Sutherland outcode for line clipping
    #[inline]
    fn cs_outcode(x: f32, y: f32, x_min: f32, y_min: f32, x_max: f32, y_max: f32) -> u8 {
        let mut code = 0u8;
        if x < x_min {
            code |= 1;
        }
        // LEFT
        else if x > x_max {
            code |= 2;
        } // RIGHT
        if y < y_min {
            code |= 4;
        }
        // TOP
        else if y > y_max {
            code |= 8;
        } // BOTTOM
        code
    }

    /// Clip a line segment to a rectangle using Cohen-Sutherland.
    /// Returns Some((x0,y0,x1,y1)) if any portion is visible, None if fully outside.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn clip_line_segment(
        mut x0: f32,
        mut y0: f32,
        mut x1: f32,
        mut y1: f32,
        x_min: f32,
        y_min: f32,
        x_max: f32,
        y_max: f32,
    ) -> Option<(f32, f32, f32, f32)> {
        let mut code0 = Self::cs_outcode(x0, y0, x_min, y_min, x_max, y_max);
        let mut code1 = Self::cs_outcode(x1, y1, x_min, y_min, x_max, y_max);

        loop {
            if (code0 | code1) == 0 {
                // Both inside
                return Some((x0, y0, x1, y1));
            }
            if (code0 & code1) != 0 {
                // Both on same outside side
                return None;
            }
            // Pick the point that is outside
            let code_out = if code0 != 0 { code0 } else { code1 };
            let (x, y);
            if code_out & 8 != 0 {
                // Below
                x = x0 + (x1 - x0) * (y_max - y0) / (y1 - y0);
                y = y_max;
            } else if code_out & 4 != 0 {
                // Above
                x = x0 + (x1 - x0) * (y_min - y0) / (y1 - y0);
                y = y_min;
            } else if code_out & 2 != 0 {
                // Right
                y = y0 + (y1 - y0) * (x_max - x0) / (x1 - x0);
                x = x_max;
            } else {
                // Left
                y = y0 + (y1 - y0) * (x_min - x0) / (x1 - x0);
                x = x_min;
            }
            if code_out == code0 {
                x0 = x;
                y0 = y;
                code0 = Self::cs_outcode(x0, y0, x_min, y_min, x_max, y_max);
            } else {
                x1 = x;
                y1 = y;
                code1 = Self::cs_outcode(x1, y1, x_min, y_min, x_max, y_max);
            }
        }
    }

    pub fn displayed_line_spans(&self, index: usize) -> Option<&[ferrite_render::LineSpan]> {
        self.line_suppression
            .current()
            .and_then(|plan| plan.spans(index))
    }
    pub fn temporal_conditions_changed(&self, context: &RenderContext) -> bool {
        self.temporal_visibility_mask.len() != context.instruction_count()
            || context
                .temporal_statuses()
                .into_iter()
                .any(|(index, visible)| self.temporal_visibility_mask.get(index) != Some(&visible))
    }

    pub fn temporal_visibility_counts(&self) -> (usize, usize) {
        self.temporal_visibility_counts
    }

    pub fn line_visibility_counts(&self) -> (usize, usize) {
        self.line_suppression
            .current()
            .map(|p| (p.fully_suppressed.len(), p.partial.len()))
            .unwrap_or_default()
    }
    fn add_line(
        &mut self,
        line: &ferrite_render::LineInstruction,
        scaler: &ferrite_render::Scaler,
        spans: Option<&[ferrite_render::LineSpan]>,
    ) {
        if !line.style.has_visible_stroke() {
            return;
        }
        for points in line.render_paths(scaler) {
            self.add_line_points(line, scaler, &points, spans);
        }
    }
    fn add_line_points(
        &mut self,
        line: &ferrite_render::LineInstruction,
        scaler: &ferrite_render::Scaler,
        points: &[ferrite_render::WorldPoint],
        spans: Option<&[ferrite_render::LineSpan]>,
    ) {
        let vertex_start = self.line_vertices.len();
        if points.len() < 2 {
            return;
        }

        // Source AABB alone cannot cull a screen-offset line.
        if line.style.offset_mm == 0. {
            let mut ax = f64::MAX;
            let mut ay = f64::MAX;
            let mut bx = f64::MIN;
            let mut by = f64::MIN;
            for p in points.iter() {
                if p.x < ax {
                    ax = p.x;
                }
                if p.y < ay {
                    ay = p.y;
                }
                if p.x > bx {
                    bx = p.x;
                }
                if p.y > by {
                    by = p.y;
                }
            }
            if !self.is_aabb_visible(ax, ay, bx, by) {
                return;
            }
        }

        let color = line.style.color.to_array();
        let width = line
            .style
            .physical_width(SCREEN_PX_PER_MM * self.state.window.scale_factor() as f32);
        if width == 0.0 {
            return;
        }

        let offset_px = line.style.offset_mm
            * (SCREEN_PX_PER_MM * self.state.window.scale_factor() as f32) as f64;
        let offsets = if offset_px == 0. {
            None
        } else {
            let projected: Vec<_> = points
                .iter()
                .map(|p| {
                    let s = scaler.world_to_screen(*p);
                    [s.x as f64, s.y as f64]
                })
                .collect();
            match ferrite_kernel::line_offset::screen_line_offsets(
                &projected,
                offset_px,
                projected.len() > 2 && projected.first() == projected.last(),
            ) {
                Ok(offsets) => Some(offsets),
                Err(error) => {
                    tracing::warn!("Physical line offset: {}", error);
                    return;
                }
            }
        };
        let shifted = |mut point: ferrite_render::ScreenPoint, segment: usize, fraction: f64| {
            if let Some(o) = &offsets {
                point.x += (o[segment][0] + fraction * (o[segment + 1][0] - o[segment][0])) as f32;
                point.y += (o[segment][1] + fraction * (o[segment + 1][1] - o[segment][1])) as f32;
            }
            point
        };
        let styled = ferrite_render::dash_line_spans(points, scaler, &line.style, spans);
        let spans = styled.as_deref().or(spans);

        // Screen-space clip bounds with generous margin for line width
        let vw = scaler.viewport.width;
        let vh = scaler.viewport.height;
        let margin = width * 2.0 + (offset_px.abs() * 4.) as f32 + 50.0; // extra margin for thick lines
        let clip_x_min = scaler.viewport.x - margin;
        let clip_y_min = scaler.viewport.y - margin;
        let clip_x_max = scaler.viewport.x + vw + margin;
        let clip_y_max = scaler.viewport.y + vh + margin;

        let clip = [clip_x_min, clip_y_min, clip_x_max, clip_y_max];
        if let Some(spans) = spans {
            for span in spans {
                if let Some((a, b)) = span.screen_endpoints(points, scaler) {
                    self.add_line_span(
                        shifted(a, span.segment, span.start),
                        shifted(b, span.segment, span.end),
                        color,
                        width,
                        clip,
                    );
                }
            }
        } else {
            let mut prev = shifted(scaler.world_to_screen(points[0]), 0, 0.);
            for (segment, p) in points[1..].iter().enumerate() {
                let curr = shifted(scaler.world_to_screen(*p), segment, 1.);
                self.add_line_span(prev, curr, color, width, clip);
                prev = curr;
            }
        }
        if line.screen_ray.is_some() || line.portrayal_path.is_some() {
            let anchor = scaler.world_to_screen(line.points[0]);
            for v in &mut self.line_vertices[vertex_start..] {
                v.offset[0] += v.position[0] - anchor.x;
                v.offset[1] += v.position[1] - anchor.y;
                v.position = [anchor.x, anchor.y];
            }
        }
    }
    fn add_line_span(
        &mut self,
        prev: ferrite_render::ScreenPoint,
        curr: ferrite_render::ScreenPoint,
        color: [f32; 4],
        width: f32,
        clip: [f32; 4],
    ) {
        let [clip_x_min, clip_y_min, clip_x_max, clip_y_max] = clip;
        // Skip segments with NaN/Inf coordinates
        if !prev.x.is_finite() || !prev.y.is_finite() || !curr.x.is_finite() || !curr.y.is_finite()
        {
            return;
        }

        // Clip to viewport bounds for clean edges
        if let Some((cx0, cy0, cx1, cy1)) = Self::clip_line_segment(
            prev.x, prev.y, curr.x, curr.y, clip_x_min, clip_y_min, clip_x_max, clip_y_max,
        ) {
            let dx = cx1 - cx0;
            let dy = cy1 - cy0;
            let len = (dx * dx + dy * dy).sqrt();

            if len >= 0.001 {
                let nx = -dy / len * width * 0.5;
                let ny = dx / len * width * 0.5;

                let base_index = self.line_vertices.len() as u32;

                self.line_vertices
                    .push(LineVertex::new(cx0, cy0, -nx, -ny, color));
                self.line_vertices
                    .push(LineVertex::new(cx0, cy0, nx, ny, color));
                self.line_vertices
                    .push(LineVertex::new(cx1, cy1, nx, ny, color));
                self.line_vertices
                    .push(LineVertex::new(cx1, cy1, -nx, -ny, color));

                self.line_indices.push(base_index);
                self.line_indices.push(base_index + 1);
                self.line_indices.push(base_index + 2);
                self.line_indices.push(base_index);
                self.line_indices.push(base_index + 2);
                self.line_indices.push(base_index + 3);
            }
        }
    }

    /// Try to render point as SVG symbol, returns true if successful
    /// Get or compute symbol classification flags for a SymbolId.
    /// Cached permanently (symbol names never change).
    #[inline]
    fn get_symbol_flags(&mut self, symbol_id: SymbolId, symbol_str: &str) -> u8 {
        if let Some(&flags) = self.symbol_class_cache.get(&symbol_id) {
            return flags;
        }
        let flags = classify_symbol(symbol_str);
        self.symbol_class_cache.insert(symbol_id, flags);
        flags
    }

    fn ensure_symbol_texture(
        &mut self,
        symbol_id: SymbolId,
        symbol_str: &str,
        geom: &crate::SymbolGeometry,
    ) {
        if !self.symbol_textures.contains_key(&symbol_id) {
            self.symbol_textures.insert(
                symbol_id,
                create_symbol_texture(&self.state, &self.pipelines, symbol_str, geom),
            );
        }
    }

    pub fn prepare_raster_scene_publication(
        &self,
        raster: PreparedRasterPublication,
    ) -> Result<PreparedRasterScenePublication> {
        self.validate_raster_publication(&raster)?;
        Ok(PreparedRasterScenePublication { raster })
    }

    pub fn validate_raster_scene_publication(
        &self,
        prepared: &PreparedRasterScenePublication,
    ) -> Result<()> {
        self.validate_raster_publication(&prepared.raster)
    }

    pub fn commit_raster_scene_publication(&mut self, prepared: PreparedRasterScenePublication) {
        assert!(
            self.validate_raster_scene_publication(&prepared).is_ok(),
            "Stale raster scene publication"
        );
        self.commit_raster_publication(prepared.raster);
        if self.continuous_layer_count == 0 {
            self.reset_pan_offset();
        }
    }

    fn try_add_symbol(
        &mut self,
        point: &ferrite_render::PointInstruction,
        scaler: &ferrite_render::Scaler,
        symbol_cache: &mut SymbolCache,
        color_profile: &ColorProfile,
        pre_interned_id: SymbolId,
    ) -> bool {
        let symbol_str = &point.symbol_ref;
        if symbol_str.is_empty() {
            return false;
        }

        // Use pre-interned SymbolId (avoids redundant read-lock)
        let symbol_id = pre_interned_id;

        // Get symbol geometry from cache (this will render via resvg if not cached)
        // Use reference to avoid cloning the pixel buffer
        let geom = match symbol_cache.get_symbol(symbol_str, color_profile) {
            Some(g) => g,
            None => return false,
        };

        self.ensure_symbol_texture(symbol_id, symbol_str, geom);

        let rotation = match ferrite_render::flat_rotation(point, scaler) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("Symbol {}: {}", symbol_str, error);
                return false;
            }
        };
        // Convert screen position
        let mut screen = match point
            .portrayal_origin
            .flat_glyph_anchor(point.position, scaler)
        {
            Ok(p) => p,
            Err(error) => {
                tracing::warn!("Symbol device anchor rejected: {error}");
                return false;
            }
        };
        let anchor = [screen.x, screen.y];
        let mm_to_px = (96.0 / 25.4) * self.state.window.scale_factor() as f32;
        screen.x += point.local_offset.0 * mm_to_px;
        screen.y -= point.local_offset.1 * mm_to_px;

        // === STAGE 1: World-coordinate deduplication ===
        // Remove exact duplicates from multiple charts at the same geographic position
        // Use high precision (6 decimal places ≈ 0.1 meter) for deduplication
        let world_x_key = (point.position.x * 1_000_000.0) as i64;
        let world_y_key = (point.position.y * 1_000_000.0) as i64;
        // Use interned symbol ID as hash (already unique per symbol type)
        let symbol_hash = symbol_id.0 as u64
            ^ (rotation.to_bits() as u64).rotate_left(13)
            ^ (point.local_offset.0.to_bits() as u64).rotate_left(29)
            ^ (point.local_offset.1.to_bits() as u64).rotate_left(47);
        let world_key = (world_x_key, world_y_key, symbol_hash);

        if !point.portrayal_origin.is_device_fixed() && self.world_dedup.contains(&world_key) {
            // Exact duplicate from another chart - skip
            return true;
        }
        if !point.portrayal_origin.is_device_fixed() {
            self.world_dedup.insert(world_key);
        }

        // === STAGE 2: World-coordinate-based decluttering ===
        // Uses world coordinates divided by pixel-equivalent cell sizes for stable grids.
        // Unlike screen-space grids, world-coordinate grids produce identical results
        // regardless of pan offset, eliminating symbol pop-in/pop-out during drag.
        //
        // Cell sizes are computed as: screen_cell_size_px / scale_factor
        // This gives the same visual density as screen-space but is pan-stable.

        // Classify symbol types via cached bitflags (O(1) lookup vs repeated starts_with)
        let flags = self.get_symbol_flags(symbol_id, symbol_str);
        let is_safety_hazard = flags & SYM_SAFETY != 0;
        let is_sounding = flags & SYM_SOUNDING != 0;

        // Compute world-space cell sizes from screen-space pixel sizes
        let scale_x = scaler.scale_x().abs();
        let scale_y = scaler.scale_y().abs();

        // Safety hazard symbols are NEVER decluttered.
        // S-100 does not define symbol decluttering (only sounding collision via champion).
        // Hiding safety symbols (wrecks, obstructions, dangers) would violate navigation safety.
        // Scale-dependent visibility is handled by ScaleMinimum/ScaleMaximum from Lua rules.

        // Skip decluttering during animation to prevent symbols from disappearing
        // Only world-coordinate deduplication (Stage 1) applies during drag/inertia
        if !point.portrayal_origin.is_device_fixed()
            && !is_safety_hazard
            && !self.skip_screen_declutter
            && scale_x > 1e-10
            && scale_y > 1e-10
        {
            // Soundings: S-100 collision avoidance (champion = shallowest wins for safety)
            // Two-stage approach:
            // 1. sounding_exact_positions: tracks exact world positions to allow all digits of same sounding
            // 2. sounding_screen_grid: world-based grid to filter out visually nearby soundings
            // At high zoom (cell_size == 0), skip grid filtering and show all soundings
            if is_sounding && self.sounding_cell_size_px > 0.1 {
                // World-space key for exact position (all digits of one sounding share this)
                let exact_key = (world_x_key, world_y_key);

                // Check if we've already allowed a sounding at this exact world position
                if self.sounding_exact_positions.contains(&exact_key) {
                    // This is another digit of an already-allowed sounding - let it through
                    // (skip grid check)
                } else {
                    // First time seeing this exact position - check world-based grid
                    let world_cell_x = self.sounding_cell_size_px as f64
                        * self.state.window.scale_factor()
                        / scale_x;
                    let world_cell_y = self.sounding_cell_size_px as f64
                        * self.state.window.scale_factor()
                        / scale_y;
                    let sounding_grid_x = (point.position.x / world_cell_x).floor() as i32;
                    let sounding_grid_y = (scaler.projection().project_y(point.position.y)
                        / world_cell_y)
                        .floor() as i32;
                    let sounding_grid_key = (sounding_grid_x, sounding_grid_y);

                    // Get current sounding's depth (default to MAX if not set)
                    let current_depth = point.depth().unwrap_or(f64::MAX);

                    if let Some(&(old_exact_key, old_depth)) =
                        self.sounding_screen_grid.get(&sounding_grid_key)
                    {
                        // Another sounding already claimed this cell
                        // For SAFETY: keep the SHALLOWEST (lowest numerical depth) sounding
                        if current_depth < old_depth {
                            // This sounding is shallower - replace the old one
                            self.sounding_exact_positions.remove(&old_exact_key);
                            self.sounding_screen_grid
                                .insert(sounding_grid_key, (exact_key, current_depth));
                            self.sounding_exact_positions.insert(exact_key);
                        } else {
                            // Existing sounding is shallower or equal - skip this one
                            return true;
                        }
                    } else {
                        // Cell is empty - this sounding claims it
                        self.sounding_screen_grid
                            .insert(sounding_grid_key, (exact_key, current_depth));
                        self.sounding_exact_positions.insert(exact_key);
                    }
                }
            }
        }

        // Add symbol instance for rendering (uses interned SymbolId - 4 bytes vs 24+ for String)
        self.symbol_instances.push(SymbolInstance {
            source: self.emitting_coverage_source,
            plane: point
                .display_plane
                .composition_plane(CompositionStage::Chart),
            feature_id: point.feature_id,
            cell_index: point.cell_index,
            world: point.position,
            priority: point.priority.0,
            anchor,
            symbol_id,
            screen_x: screen.x,
            screen_y: screen.y,
            scale: point.scale,
            rotation,
        });

        true
    }

    /// Record a point instruction that could not be rendered as a symbol.
    /// Empty `symbol_ref` indicates a portrayal-rule bug; missing/un-renderable
    /// symbols indicate a Portrayal Catalogue gap. Both are logged once per id
    /// so they surface in normal logs without spamming every frame.
    fn note_unrendered_symbol(&mut self, sym_id: SymbolId, symbol_ref: &str) {
        if symbol_ref.is_empty() {
            self.empty_symbol_ref_count = self.empty_symbol_ref_count.saturating_add(1);
            return;
        }
        if self.missing_symbol_ids.insert(sym_id) {
            tracing::warn!(
                "Symbol '{}' could not be rendered (missing SVG or unresolved color tokens) — \
                 check Portrayal Catalogue completeness",
                symbol_ref
            );
        }
    }

    /// Number of unique symbol ids that failed to render this session.
    pub fn missing_symbol_count(&self) -> usize {
        self.missing_symbol_ids.len()
    }

    /// Number of point instructions emitted without a symbol_ref this session.
    /// A non-zero count indicates a portrayal-rule bug.
    pub fn empty_symbol_ref_count(&self) -> u32 {
        self.empty_symbol_ref_count
    }

    /// Exact font, rotation, wrapping and collision policy shared with drawing.
    fn layout_chart_text(
        &self,
        mut shapes: Vec<ChartTextShape>,
        capture_shapes: bool,
    ) -> (Vec<ChartTextShape>, Vec<bool>) {
        let _flat_text_span = self
            .flat_span(ferrite_render::flat_reuse_diagnostics::FlatFrameStage::TextAndDeclutter);
        shapes.clear();
        let mut accepted = if capture_shapes {
            Vec::new()
        } else {
            vec![false; self.text_labels.len()]
        };
        let (width, height) = self.state.viewport_size();
        let ppp = self.egui.ctx.pixels_per_point();
        let clip =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width / ppp, height / ppp));
        // Apply the same GPU pan/zoom offset so text tracks with chart geometry during drag
        if !self.text_labels.is_empty() {
            let pan_x = self.screen_pan_offset.0;
            let pan_y = self.screen_pan_offset.1;
            let zoom = self.screen_zoom_scale;
            let (pivot_x, pivot_y) = self.screen_zoom_pivot;

            let painter = self.egui.ctx.layer_painter(egui::LayerId::background());
            let mut placement = ferrite_render::TextPlacement::default();
            let mut labels: Vec<_> = (0..self.text_labels.len()).collect();
            // Higher planes/priorities retain text on collisions; stable source
            // order provides a neutral tie break. This is a display-engine policy.
            labels.sort_by_key(|&index| {
                std::cmp::Reverse((
                    self.text_labels[index].plane,
                    self.text_labels[index].priority,
                ))
            });
            for label_index in labels {
                let label = &self.text_labels[label_index];
                let color = egui::Color32::from_rgba_unmultiplied(
                    (label.color[0] * 255.0) as u8,
                    (label.color[1] * 255.0) as u8,
                    (label.color[2] * 255.0) as u8,
                    (label.color[3] * 255.0) as u8,
                );

                // Build a LayoutJob to support bold/italic font variants
                let mut job = egui::text::LayoutJob::single_section(
                    label.text.clone(),
                    egui::TextFormat {
                        font_id: egui::FontId {
                            size: label.font_size / self.egui.ctx.pixels_per_point(),
                            family: if label.bold {
                                egui::FontFamily::Name("ChartBold".into())
                            } else {
                                egui::FontFamily::Proportional
                            },
                        },
                        color,
                        italics: label.italic,
                        ..Default::default()
                    },
                );
                job.wrap = egui::text::TextWrapping {
                    max_rows: 1,
                    break_anywhere: false,
                    ..Default::default()
                };

                let galley = painter.layout_job(job);
                let text_width = galley.rect.width();
                let text_height = galley.rect.height();

                let wrap_offsets = [0.0, -self.lon_wrap_screen_px, self.lon_wrap_screen_px];
                let copies = if self.lon_wrap_screen_px > 0.0
                    && !label
                        .source
                        .is_some_and(|s| self.device_fixed_sources.contains(&s))
                {
                    3
                } else {
                    1
                };
                for (wrap_pass, wrap_offset) in wrap_offsets[..copies].iter().enumerate() {
                    let coverage_wrap_pass = if label.source.is_some() {
                        wrap_pass as u8
                    } else {
                        0
                    };
                    // Wrap and pan before zoom, keeping local text offsets unscaled.
                    let sx = label.anchor[0] + pan_x + wrap_offset;
                    let sy = label.anchor[1] + pan_y;
                    let sx = ((sx - pivot_x) * zoom + pivot_x + label.screen_x - label.anchor[0])
                        / self.egui.ctx.pixels_per_point();
                    let sy = ((sy - pivot_y) * self.screen_zoom_scale_y + pivot_y + label.screen_y
                        - label.anchor[1])
                        / self.egui.ctx.pixels_per_point();

                    // Apply horizontal alignment
                    let x = match label.h_align {
                        ferrite_render::HAlign::Left => sx,
                        ferrite_render::HAlign::Center => sx - text_width * 0.5,
                        ferrite_render::HAlign::Right => sx - text_width,
                    };

                    // Apply vertical alignment
                    let y = match label.v_align {
                        ferrite_render::VAlign::Top => sy,
                        ferrite_render::VAlign::Middle => sy - text_height * 0.5,
                        ferrite_render::VAlign::Bottom => sy - text_height,
                    };

                    let angle = label.rotation.to_radians();
                    let dx = x - sx;
                    let dy = y - sy;
                    let origin = egui::pos2(
                        sx + dx * angle.cos() - dy * angle.sin(),
                        sy + dx * angle.sin() + dy * angle.cos(),
                    );
                    let bounds = if label.background.is_some() {
                        galley.rect.union(galley.mesh_bounds)
                    } else {
                        galley.mesh_bounds
                    };
                    if !bounds.is_finite() || !bounds.is_positive() {
                        continue;
                    }
                    let min = bounds.min;
                    let bounds_origin = [
                        origin.x + min.x * angle.cos() - min.y * angle.sin(),
                        origin.y + min.x * angle.sin() + min.y * angle.cos(),
                    ];
                    let footprint = ferrite_render::TextFootprint::rotated(
                        bounds_origin,
                        [bounds.width(), bounds.height()],
                        angle,
                    );
                    let [min, max] = footprint.bounds();
                    if !clip.intersects(egui::Rect::from_min_max(
                        egui::pos2(min[0], min[1]),
                        egui::pos2(max[0], max[1]),
                    )) {
                        continue;
                    }
                    if !placement.try_place(footprint) {
                        continue;
                    }
                    if let Some(active) = accepted.get_mut(label_index) {
                        *active = true;
                    }
                    if capture_shapes {
                        if let Some(background) = label.background {
                            let color = egui::Color32::from_rgba_unmultiplied(
                                (background[0] * 255.) as u8,
                                (background[1] * 255.) as u8,
                                (background[2] * 255.) as u8,
                                (background[3] * 255.) as u8,
                            );
                            let corners = footprint
                                .corners
                                .iter()
                                .map(|p| egui::pos2(p[0], p[1]))
                                .collect();
                            shapes.push((
                                label.plane,
                                label.priority,
                                label.source,
                                coverage_wrap_pass,
                                egui::epaint::ClippedShape {
                                    clip_rect: clip,
                                    shape: egui::Shape::convex_polygon(
                                        corners,
                                        color,
                                        egui::Stroke::NONE,
                                    ),
                                },
                            ));
                        }
                        shapes.push((
                            label.plane,
                            label.priority,
                            label.source,
                            coverage_wrap_pass,
                            egui::epaint::ClippedShape {
                                clip_rect: clip,
                                shape: egui::epaint::TextShape::new(origin, galley.clone(), color)
                                    .with_angle(angle)
                                    .into(),
                            },
                        ));
                    }
                }
            }
        }

        (shapes, accepted)
    }

    /// Shared text/selection overlay; optionally includes application panels.
    fn prepare_overlay(&mut self, include_panels: bool) -> egui::FullOutput {
        self.chart_text_shapes.clear();
        // Begin egui frame
        self.egui.begin_frame(&self.state.window);

        // Draw egui UI
        if include_panels {
            self.egui.draw_ui(&mut self.ui_state);
        }

        // Selection is an application overlay, separate from IHO portrayal symbols.
        if let Some([x, y]) = self.selection_anchor {
            let (pivot_x, pivot_y) = self.screen_zoom_pivot;
            let ppp = self.egui.ctx.pixels_per_point();
            let pos = egui::pos2(
                ((x + self.screen_pan_offset.0 - pivot_x) * self.screen_zoom_scale + pivot_x) / ppp,
                ((y + self.screen_pan_offset.1 - pivot_y) * self.screen_zoom_scale_y + pivot_y)
                    / ppp,
            );
            let (x, y, w, h) = self.ui_state.chart_area;
            let clip = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h));
            let painter = self
                .egui
                .ctx
                .layer_painter(egui::LayerId::background())
                .with_clip_rect(clip);
            for path in &self.selection_screen_geometry {
                let points: Vec<_> = path
                    .iter()
                    .map(|p| {
                        egui::pos2(
                            ((p[0] + self.screen_pan_offset.0 - pivot_x) * self.screen_zoom_scale
                                + pivot_x)
                                / ppp,
                            ((p[1] + self.screen_pan_offset.1 - pivot_y)
                                * self.screen_zoom_scale_y
                                + pivot_y)
                                / ppp,
                        )
                    })
                    .collect();
                if points.len() >= 2 {
                    painter.add(egui::Shape::line(
                        points.clone(),
                        egui::Stroke::new(5.0_f32, egui::Color32::BLACK),
                    ));
                    painter.add(egui::Shape::line(
                        points,
                        egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0, 230, 255)),
                    ));
                }
            }
            painter.circle_stroke(pos, 18.0, egui::Stroke::new(5.0_f32, egui::Color32::BLACK));
            painter.circle_stroke(
                pos,
                18.0,
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0, 230, 255)),
            );
            if let Some(f) = &self.ui_state.selected_feature {
                painter.text(
                    pos + egui::vec2(23.0, -22.0),
                    egui::Align2::LEFT_BOTTOM,
                    &f.feature_type,
                    egui::FontId::proportional(13.0),
                    egui::Color32::BLACK,
                );
            }
        }

        let shapes = std::mem::take(&mut self.chart_text_shapes);
        self.chart_text_shapes = self.layout_chart_text(shapes, true).0;

        // End egui frame and get output
        self.egui.end_frame(&self.state.window)
    }

    /// Reuse exact immutable40B payloads only. Changed content always allocates;
    /// neither live batches nor future private publications are queue-written.
    fn prepare_symbol_instance_vertex_buffer(&mut self) -> wgpu::Buffer {
        let count = self.packed_symbol_vertices.len() / 4;
        let byte_count = (count as u64) * 40;
        if let Some(cache)=self.immutable_instance_cache.as_mut().filter(|_|byte_count<=crate::immutable_payload_cache::ImmutablePayloadCache::<wgpu::Buffer>::MAX_PAYLOAD as u64) {
            let mut bytes=Vec::with_capacity(byte_count as usize);
            for quad in self.packed_symbol_vertices.as_chunks::<4>().0 {
                bytes.extend_from_slice(bytemuck::bytes_of(&crate::symbol_instance::SymbolQuadInstance::from_quad(quad)));
            }
            let state=&self.state;
            return cache.reuse_or_create(&bytes,||state.create_vertex_buffer(&bytes,"immutable_symbol_instance_vb"));
        }
        if let Some(cache) = self.immutable_instance_cache.as_mut() {
            cache.record_uncached_creation();
        }
        // OFF/oversized fallback preserves the original direct mapped upload.
        let buffer = self.state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("symbol_instance_vb"),
            size: byte_count,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: true,
        });
        {
            let mut mapped = buffer.slice(..).get_mapped_range_mut();
            for (output, quad) in mapped
                .as_chunks_mut::<40>()
                .0
                .iter_mut()
                .zip(self.packed_symbol_vertices.as_chunks::<4>().0.iter())
            {
                output.copy_from_slice(bytemuck::bytes_of(
                    &crate::symbol_instance::SymbolQuadInstance::from_quad(quad),
                ));
            }
        }
        buffer.unmap();
        buffer
    }

    /// Upload changed geometry once, shared by screen and image targets.
    fn prepare_geometry_buffers(&mut self) {
        let _flat_upload_span =
            self.flat_span(ferrite_render::flat_reuse_diagnostics::FlatFrameStage::BufferUpload);
        if self.gpu_buffers_dirty {
            self.cached_area_vb = if !self.area_vertices.is_empty() {
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.area_vertices).len() as u64,
                        );
                    }
                    self.state
                        .create_vertex_buffer(&self.area_vertices, "area_vertices")
                })
            } else {
                None
            };
            self.cached_area_ib = if !self.area_indices.is_empty() {
                self.cached_area_index_count = self.area_indices.len() as u32;
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.area_indices).len() as u64,
                        );
                    }
                    self.state
                        .create_index_buffer(&self.area_indices, "area_indices")
                })
            } else {
                self.cached_area_index_count = 0;
                None
            };

            self.cached_line_vb = if !self.line_vertices.is_empty() {
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.line_vertices).len() as u64,
                        );
                    }
                    self.state
                        .create_vertex_buffer(&self.line_vertices, "line_vertices")
                })
            } else {
                None
            };
            self.cached_line_ib = if !self.line_indices.is_empty() {
                self.cached_line_index_count = self.line_indices.len() as u32;
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.line_indices).len() as u64,
                        );
                    }
                    self.state
                        .create_index_buffer(&self.line_indices, "line_indices")
                })
            } else {
                self.cached_line_index_count = 0;
                None
            };

            self.cached_pattern_vb = if !self.pattern_vertices.is_empty() {
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.pattern_vertices).len() as u64,
                        );
                    }
                    self.state
                        .create_vertex_buffer(&self.pattern_vertices, "pattern_vertices")
                })
            } else {
                None
            };
            self.cached_pattern_ib = if !self.pattern_indices.is_empty() {
                self.cached_pattern_index_count = self.pattern_indices.len() as u32;
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.pattern_indices).len() as u64,
                        );
                    }
                    self.state
                        .create_index_buffer(&self.pattern_indices, "pattern_indices")
                })
            } else {
                self.cached_pattern_index_count = 0;
                None
            };

            // World map separate GPU buffers
            self.cached_wm_line_vb = if !self.world_map_line_vertices.is_empty() {
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.world_map_line_vertices).len()
                                as u64,
                        );
                    }
                    self.state
                        .create_vertex_buffer(&self.world_map_line_vertices, "wm_line_vb")
                })
            } else {
                None
            };
            self.cached_wm_line_ib = if !self.world_map_line_indices.is_empty() {
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.world_map_line_indices).len()
                                as u64,
                        );
                    }
                    self.state
                        .create_index_buffer(&self.world_map_line_indices, "wm_line_ib")
                })
            } else {
                None
            };
            self.cached_wm_mask_vb = if !self.world_map_mask_vertices.is_empty() {
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.world_map_mask_vertices).len()
                                as u64,
                        );
                    }
                    self.state
                        .create_vertex_buffer(&self.world_map_mask_vertices, "wm_mask_vb")
                })
            } else {
                None
            };
            self.cached_wm_mask_ib = if !self.world_map_mask_indices.is_empty() {
                Some({
                    if let Some(cell) = &self.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(&self.world_map_mask_indices).len()
                                as u64,
                        );
                    }
                    self.state
                        .create_index_buffer(&self.world_map_mask_indices, "wm_mask_ib")
                })
            } else {
                None
            };

            self.gpu_buffers_dirty = false;
        }
        // Pre-build all symbol GPU buffers before the render pass.
        // This avoids mutable self borrows inside the render pass where view bind groups
        // are held as immutable references (for longitude wrapping multi-pass rendering).
        {
            // Copy one tuple before mutable packing. Packing changes only the
            // packed output arrays; it never changes symbol_priority_ranges.
            let range_count = self.symbol_priority_ranges.len();
            // Bound the transient lookup; oversized charts use the original scan.
            let mut symbol_keys = (self.draw_range_index_enabled
                && !false
                && self.cached_symbol_buffers.len() <= 32_768
                && range_count <= 32_768)
                .then(|| {
                    self.cached_symbol_buffers
                        .iter()
                        .map(|(p, q, a, b, _, _, _)| (*p, *q, *a, *b))
                        .collect::<FxHashSet<_>>()
                });
            for range_index in 0..range_count {
                let (pl, pri, start, end, _) = self.symbol_priority_ranges[range_index];
                if end <= start {
                    continue;
                }
                let already_cached = match &symbol_keys {
                    Some(keys) => keys.contains(&(pl, pri, start, end)),
                    None => self
                        .cached_symbol_buffers
                        .iter()
                        .any(|(cp, cpr, cs, ce, _, _, _)| {
                            *cp == pl && *cpr == pri && *cs == start && *ce == end
                        }),
                };
                if already_cached {
                    continue;
                }
                self.pack_symbol_batch_range(start, end);
                if self.packed_symbol_indices.is_empty() {
                    continue;
                }
                let (sym_vb, sym_ib) = if self.pipelines.symbol_instance_pipeline.is_some() {
                    let buffer = self.prepare_symbol_instance_vertex_buffer();
                    // Buffer::clone retains the same immutable GPU handle; INDEX-only
                    // usage deliberately omits COPY_DST, preventing pool-style writes.
                    if self.shared_symbol_quad_index_buffer.is_none() {
                        self.shared_symbol_quad_index_buffer =
                            Some(self.state.create_index_buffer(
                                &crate::symbol_instance::QUAD_INDICES,
                                "shared_symbol_instance_ib",
                            ));
                    }
                    (
                        buffer,
                        self.shared_symbol_quad_index_buffer
                            .as_ref()
                            .expect("quad initialized")
                            .clone(),
                    )
                } else {
                    (
                        {
                            if let Some(cell) = &self.flat_diagnostic {
                                let mut row = cell.borrow_mut();
                                row.work.buffer_upload_calls =
                                    row.work.buffer_upload_calls.saturating_add(1);
                                row.work.buffer_upload_bytes =
                                    row.work.buffer_upload_bytes.saturating_add(
                                        bytemuck::cast_slice::<_, u8>(&self.packed_symbol_vertices)
                                            .len() as u64,
                                    );
                            }
                            self.state.create_vertex_buffer(
                                &self.packed_symbol_vertices,
                                "symbol_packed_vb",
                            )
                        },
                        {
                            if let Some(cell) = &self.flat_diagnostic {
                                let mut row = cell.borrow_mut();
                                row.work.buffer_upload_calls =
                                    row.work.buffer_upload_calls.saturating_add(1);
                                row.work.buffer_upload_bytes =
                                    row.work.buffer_upload_bytes.saturating_add(
                                        bytemuck::cast_slice::<_, u8>(&self.packed_symbol_indices)
                                            .len() as u64,
                                    );
                            }
                            self.state.create_index_buffer(
                                &self.packed_symbol_indices,
                                "symbol_packed_ib",
                            )
                        },
                    )
                };
                let ranges: Vec<_> = self.packed_symbol_ranges.clone();
                self.cached_symbol_buffers
                    .push((pl, pri, start, end, sym_vb, sym_ib, ranges));
                if let Some(keys) = &mut symbol_keys {
                    keys.insert((pl, pri, start, end));
                }
            }
        }
    }

    /// Tessellate each ordered glyph group into its own immutable GPU buffers.
    /// The egui atlas is shared; no per-priority atlas copies or repeated job scans.
    fn prepare_chart_text(&mut self, output: &mut egui::FullOutput) {
        self.egui
            .prepare_chart_atlas(&self.state.device, &self.state.queue, output);
        self.chart_text_meshes.clear();
        self.chart_text_buffers.begin();
        let mut groups = std::collections::BTreeMap::<
            (CompositionPlane, i32, Option<usize>, u8),
            Vec<egui::epaint::ClippedShape>,
        >::new();
        for (plane, priority, source, wrap_pass, shape) in self.chart_text_shapes.drain(..) {
            groups
                .entry((plane, priority, source, wrap_pass))
                .or_default()
                .push(shape);
        }
        let ppp = output.pixels_per_point;
        for ((plane, priority, source, wrap_pass), shapes) in groups {
            for job in self.egui.ctx.tessellate(shapes, ppp) {
                if let egui::epaint::Primitive::Mesh(mesh) = job.primitive {
                    if mesh.indices.is_empty() {
                        continue;
                    }
                    let Some(bind_group) = self.egui.chart_atlas_bind_group(mesh.texture_id) else {
                        tracing::error!("Chart font atlas {:?} missing", mesh.texture_id);
                        continue;
                    };
                    let [width, height] = [self.state.size.width, self.state.size.height];
                    let x = (job.clip_rect.min.x * ppp).round().clamp(0., width as f32) as u32;
                    let y = (job.clip_rect.min.y * ppp).round().clamp(0., height as f32) as u32;
                    let right = (job.clip_rect.max.x * ppp)
                        .round()
                        .clamp(x as f32, width as f32) as u32;
                    let bottom = (job.clip_rect.max.y * ppp)
                        .round()
                        .clamp(y as f32, height as f32) as u32;
                    if x == right || y == bottom {
                        continue;
                    }
                    let scissor = [x, y, right - x, bottom - y];
                    let vertices: Vec<_> = mesh
                        .vertices
                        .iter()
                        .map(|v| crate::ChartTextVertex {
                            position: [v.pos.x * ppp, v.pos.y * ppp],
                            uv: [v.uv.x, v.uv.y],
                            color: v.color.to_array(),
                        })
                        .collect();
                    let (vertex_buffer, index_buffer) = self.chart_text_buffers.upload(
                        &self.state.device,
                        &self.state.queue,
                        &vertices,
                        &mesh.indices,
                    );
                    self.chart_text_meshes.push(GpuChartText {
                        source,
                        wrap_pass,
                        plane,
                        priority,
                        scissor,
                        bind_group,
                        vertices: vertex_buffer,
                        indices: index_buffer,
                        index_count: mesh.indices.len() as u32,
                    });
                }
            }
        }
    }

    pub fn chart_text_buffer_statistics(&self) -> serde_json::Value {
        serde_json::json!({"allocations":self.chart_text_buffers.allocations,"writes":self.chart_text_buffers.writes,"retained_bytes":self.chart_text_buffers.retained_bytes,"budget_bytes":ChartTextBufferPool::BUDGET,"slots":self.chart_text_buffers.slots.len()})
    }

    /// Default 4x matches chart antialiasing; 1x permits controlled sampling audits.
    /// Disable retained area meshes for constrained hosts or differential audits.
    /// The next preparation releases cached areas and executes identical cold draping.
    /// Disable only immutable source-curve preparation for controlled differential tests.
    /// Same-binary diagnostic control; only broad-phase selection changes.
    pub fn set_spatial_hierarchy_enabled(&mut self, enabled: bool) {
        self.spatial_hierarchy_enabled = enabled;
    }

    /// Encode the same ordered geometry pass for every render target.
    fn bind_coverage_pipeline<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        primitive: CoveragePrimitive,
        source: Option<usize>,
        wrap_pass: usize,
    ) -> bool {
        use crate::coverage_gpu_frame::CoverageGpuBinding;
        if self.coverage_failed
            || (wrap_pass != 0 && source.is_some_and(|s| self.device_fixed_sources.contains(&s)))
        {
            return false;
        }
        let binding = match source {
            None => CoverageGpuBinding::Unclipped,
            Some(index) => match (&self.coverage_frame, &self.prepared_coverage) {
                (Some(frame), Some(prepared)) => {
                    match frame.resolve_instruction(prepared, wrap_pass, index) {
                        Ok(binding) => binding,
                        Err(error) => {
                            tracing::error!("Coverage draw rejected: {error}");
                            return false;
                        }
                    }
                }
                (None, None) if self.device_fixed_sources.contains(&index) => {
                    CoverageGpuBinding::Unclipped
                }
                _ => return false,
            },
        };
        match binding {
            CoverageGpuBinding::Hidden => false,
            CoverageGpuBinding::Unclipped => {
                pass.set_pipeline(match primitive {
                    CoveragePrimitive::Area => &self.pipelines.area_pipeline,
                    CoveragePrimitive::Line => &self.pipelines.line_pipeline,
                    CoveragePrimitive::Symbol => self
                        .pipelines
                        .symbol_instance_pipeline
                        .as_ref()
                        .unwrap_or(&self.pipelines.texture_pipeline),
                    CoveragePrimitive::Pattern => &self.pipelines.pattern_fill_pipeline,
                    CoveragePrimitive::Text => &self.pipelines.chart_text_pipeline,
                });
                true
            }
            CoverageGpuBinding::Masked(group) => {
                let Some(pipelines) = &self.coverage_pipelines else {
                    return false;
                };
                pass.set_pipeline(match primitive {
                    CoveragePrimitive::Area => &pipelines.area,
                    CoveragePrimitive::Line => &pipelines.line,
                    CoveragePrimitive::Symbol => pipelines
                        .symbol_instance
                        .as_ref()
                        .unwrap_or(&pipelines.symbol),
                    CoveragePrimitive::Pattern => &pipelines.pattern,
                    CoveragePrimitive::Text => &pipelines.text,
                });
                if matches!(primitive, CoveragePrimitive::Area | CoveragePrimitive::Line) {
                    pass.set_bind_group(1, &pipelines.empty_asset, &[]);
                }
                pass.set_bind_group(2, group, &[]);
                true
            }
        }
    }

    fn encode_chart(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        resolve_target: Option<&wgpu::TextureView>,
        gpu_query: Option<&wgpu_profiler::GpuProfilerQuery>,
    ) {
        let draw_index = if self.draw_range_index_enabled && !false {
            let total = self
                .area_priority_ranges
                .len()
                .checked_add(self.pattern_ranges.len())
                .and_then(|n| n.checked_add(self.line_priority_ranges.len()))
                .and_then(|n| n.checked_add(self.symbol_priority_ranges.len()))
                .and_then(|n| n.checked_add(self.chart_text_meshes.len()))
                .and_then(|n| n.checked_add(self.raster_layers.len()));
            total.and_then(DrawRangeIndex::new).map(|mut index| {
                for (i, &(p, q, _, _, _)) in self.area_priority_ranges.iter().enumerate() {
                    index.push(DrawKind::Area, (p, q), i);
                }
                for (i, &(p, q, _, _, _, _, _)) in self.pattern_ranges.iter().enumerate() {
                    index.push(DrawKind::Pattern, (p, q), i);
                }
                for (i, &(p, q, _, _, _)) in self.line_priority_ranges.iter().enumerate() {
                    index.push(DrawKind::Line, (p, q), i);
                }
                for (i, &(p, q, _, _, _)) in self.symbol_priority_ranges.iter().enumerate() {
                    index.push(DrawKind::Symbol, (p, q), i);
                }
                for (i, text) in self.chart_text_meshes.iter().enumerate() {
                    index.push(DrawKind::Text, (text.plane, text.priority), i);
                }
                for (i, layer) in self.raster_layers.iter().enumerate() {
                    index.push(DrawKind::Raster, layer.draw_order.render_key(), i);
                }
                index.finish();
                index
            })
        } else {
            None
        };
        let draw_index = draw_index.as_ref();
        // Preserve position() first-match semantics, including duplicate keys.
        let symbol_lookup = draw_index
            .filter(|_| self.cached_symbol_buffers.len() <= 32_768)
            .map(|_| {
                let mut lookup = FxHashMap::default();
                for (i, (p, q, start, end, _, _, _)) in
                    self.cached_symbol_buffers.iter().enumerate()
                {
                    lookup.entry((*p, *q, *start, *end)).or_insert(i);
                }
                lookup
            });
        let bg = self.background_color.to_array();
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: gpu_query.and_then(|query| query.render_pass_timestamp_writes()),
            });

            let extent = [self.state.size.width, self.state.size.height];
            let viewport = self.chart_geometry_viewport.unwrap_or_else(|| {
                ferrite_render::Viewport::new(extent[0] as f32, extent[1] as f32)
            });
            let Some(chart_scissor) = chart_pass_scissor(viewport, extent) else {
                return;
            };
            render_pass.set_scissor_rect(
                chart_scissor[0],
                chart_scissor[1],
                chart_scissor[2],
                chart_scissor[3],
            );

            // === LAYER 1: World map coastlines (lowest layer) ===
            if let (Some(vb), Some(ib)) = (&self.cached_wm_line_vb, &self.cached_wm_line_ib) {
                let idx_count = self.world_map_line_indices.len() as u32;
                if idx_count > 0 {
                    render_pass.set_pipeline(&self.pipelines.line_pipeline);
                    render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                    render_pass.set_vertex_buffer(0, vb.slice(..));
                    render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..idx_count, 0, 0..1);
                }
            }

            // === LAYER 2: Opaque background rectangles over chart bboxes (mask coastlines) ===
            if let (Some(vb), Some(ib)) = (&self.cached_wm_mask_vb, &self.cached_wm_mask_ib) {
                let idx_count = self.world_map_mask_indices.len() as u32;
                if idx_count > 0 {
                    render_pass.set_pipeline(&self.pipelines.area_pipeline);
                    render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                    render_pass.set_vertex_buffer(0, vb.slice(..));
                    render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..idx_count, 0, 0..1);
                }
            }

            // === LAYER 3: Chart data (S-101 priority-based rendering) ===
            // Drawn at center, left (-360°), and right (+360°) offsets for wrapping.
            let mut priority_set = FxHashSet::default();
            for &(plane, pri, _, _, _) in &self.area_priority_ranges {
                priority_set.insert((plane, pri));
            }
            for &(plane, pri, _, _, _) in &self.line_priority_ranges {
                priority_set.insert((plane, pri));
            }
            for &(plane, pri, _, _, _) in &self.symbol_priority_ranges {
                priority_set.insert((plane, pri));
            }
            for &(plane, pri, _, _, _, _, _) in &self.pattern_ranges {
                priority_set.insert((plane, pri));
            }
            for text in &self.chart_text_meshes {
                priority_set.insert((text.plane, text.priority));
            }
            for layer in &self.raster_layers {
                if ferrite_render::raster_groups_visible(
                    &layer.viewing_groups,
                    self.raster_enabled_groups.as_ref(),
                ) {
                    priority_set.insert(layer.draw_order.render_key());
                }
            }
            let mut all_priorities: Vec<(CompositionPlane, i32)> =
                priority_set.into_iter().collect();
            all_priorities.sort_unstable();

            // Number of wrapping passes: center (always) + left/right if wrapping
            let wrap_pass_count: u8 = if self.lon_wrap_screen_px > 0.0 { 3 } else { 1 };

            // Complete each priority across longitude copies before advancing.
            // Otherwise a low-priority wrapped copy can cover an earlier high-priority copy.
            for &(plane, priority) in &all_priorities {
                for wrap_pass in 0..wrap_pass_count {
                    let view_bg = match wrap_pass {
                        1 => &self.view_bind_group_left,
                        2 => &self.view_bind_group_right,
                        _ => &self.view_bind_group,
                    };
                    self.draw_rasters(&mut render_pass, view_bg, (plane, priority), draw_index);
                    // Render areas for this priority
                    if let (Some(vb), Some(ib)) = (&self.cached_area_vb, &self.cached_area_ib) {
                        for range_index in selected_indices(
                            draw_index,
                            DrawKind::Area,
                            (plane, priority),
                            self.area_priority_ranges.len(),
                        ) {
                            let (pl, pri, start, end, source) =
                                self.area_priority_ranges[range_index];
                            if pl == plane && pri == priority && end > start {
                                if !self.bind_coverage_pipeline(
                                    &mut render_pass,
                                    CoveragePrimitive::Area,
                                    source,
                                    wrap_pass as usize,
                                ) {
                                    continue;
                                }
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, vb.slice(..));
                                render_pass
                                    .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                render_pass.draw_indexed(start as u32..end as u32, 0, 0..1);
                            }
                        }
                    }

                    // Render pattern fills for this priority
                    if let (Some(vb), Some(ib)) = (&self.cached_pattern_vb, &self.cached_pattern_ib)
                    {
                        for range_index in selected_indices(
                            draw_index,
                            DrawKind::Pattern,
                            (plane, priority),
                            self.pattern_ranges.len(),
                        ) {
                            let (pl, pri, start, end, pat_key, wrap_mode, source) =
                                &self.pattern_ranges[range_index];
                            if *pl == plane
                                && *pri == priority
                                && end > start
                                && (*wrap_mode == 255 || *wrap_mode == wrap_pass)
                            {
                                if let Some(pat_tex) = self.pattern_textures.get(pat_key) {
                                    if !self.bind_coverage_pipeline(
                                        &mut render_pass,
                                        CoveragePrimitive::Pattern,
                                        *source,
                                        wrap_pass as usize,
                                    ) {
                                        continue;
                                    }
                                    render_pass.set_bind_group(
                                        0,
                                        if *wrap_mode == 255 {
                                            view_bg
                                        } else {
                                            &self.view_bind_group
                                        },
                                        &[],
                                    );
                                    render_pass.set_bind_group(1, &pat_tex.bind_group, &[]);
                                    render_pass.set_vertex_buffer(0, vb.slice(..));
                                    render_pass
                                        .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                    render_pass.draw_indexed(*start as u32..*end as u32, 0, 0..1);
                                }
                            }
                        }
                    }

                    // Render lines for this priority
                    if let (Some(vb), Some(ib)) = (&self.cached_line_vb, &self.cached_line_ib) {
                        for range_index in selected_indices(
                            draw_index,
                            DrawKind::Line,
                            (plane, priority),
                            self.line_priority_ranges.len(),
                        ) {
                            let (pl, pri, start, end, source) =
                                self.line_priority_ranges[range_index];
                            if pl == plane && pri == priority && end > start {
                                if !self.bind_coverage_pipeline(
                                    &mut render_pass,
                                    CoveragePrimitive::Line,
                                    source,
                                    wrap_pass as usize,
                                ) {
                                    continue;
                                }
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, vb.slice(..));
                                render_pass
                                    .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                render_pass.draw_indexed(start as u32..end as u32, 0, 0..1);
                            }
                        }
                    }

                    // Render symbols for this priority (pre-built GPU buffers)
                    for range_index in selected_indices(
                        draw_index,
                        DrawKind::Symbol,
                        (plane, priority),
                        self.symbol_priority_ranges.len(),
                    ) {
                        let (pl, pri, start, end, source) =
                            self.symbol_priority_ranges[range_index];
                        if pl == plane && pri == priority && end > start {
                            // Find pre-built buffer (built before render pass)
                            let cache_idx = match &symbol_lookup {
                                Some(lookup) => lookup.get(&(pl, pri, start, end)).copied(),
                                None => self.cached_symbol_buffers.iter().position(
                                    |(cp, cpr, cs, ce, _, _, _)| {
                                        *cp == pl && *cpr == pri && *cs == start && *ce == end
                                    },
                                ),
                            };
                            if let Some(buf_idx) = cache_idx {
                                let (_, _, _, _, ref sym_vb, ref sym_ib, ref ranges) =
                                    self.cached_symbol_buffers[buf_idx];

                                if !self.bind_coverage_pipeline(
                                    &mut render_pass,
                                    CoveragePrimitive::Symbol,
                                    source,
                                    wrap_pass as usize,
                                ) {
                                    continue;
                                }
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, sym_vb.slice(..));
                                render_pass
                                    .set_index_buffer(sym_ib.slice(..), wgpu::IndexFormat::Uint32);

                                for &(sym_id, idx_start, idx_count) in ranges {
                                    if let Some(tex) = self.symbol_textures.get(&sym_id) {
                                        render_pass.set_bind_group(1, &tex.bind_group, &[]);
                                        if self.pipelines.symbol_instance_pipeline.is_some() {
                                            // Original six indices per accepted symbol map to consecutive instances.
                                            render_pass.draw_indexed(
                                                0..6,
                                                0,
                                                idx_start / 6..(idx_start + idx_count) / 6,
                                            );
                                        } else {
                                            render_pass.draw_indexed(
                                                idx_start..idx_start + idx_count,
                                                0,
                                                0..1,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for text_index in selected_indices(
                    draw_index,
                    DrawKind::Text,
                    (plane, priority),
                    self.chart_text_meshes.len(),
                ) {
                    let text = &self.chart_text_meshes[text_index];
                    if text.plane == plane && text.priority == priority {
                        let Some([x, y, width, height]) =
                            intersect_scissors(text.scissor, chart_scissor)
                        else {
                            continue;
                        };
                        render_pass.set_scissor_rect(x, y, width, height);
                        if !self.bind_coverage_pipeline(
                            &mut render_pass,
                            CoveragePrimitive::Text,
                            text.source,
                            text.wrap_pass as usize,
                        ) {
                            continue;
                        }
                        render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                        render_pass.set_bind_group(1, &text.bind_group, &[]);
                        render_pass.set_vertex_buffer(0, text.vertices.slice(..));
                        render_pass
                            .set_index_buffer(text.indices.slice(..), wgpu::IndexFormat::Uint32);
                        render_pass.draw_indexed(0..text.index_count, 0, 0..1);
                    }
                }
                // Text has its own clip; the next priority must return to the
                // chart pane, not to the full surface behind application chrome.
                render_pass.set_scissor_rect(
                    chart_scissor[0],
                    chart_scissor[1],
                    chart_scissor[2],
                    chart_scissor[3],
                );
            }
        }
    }

    /// GPU chart execution only; this excludes UI, acquisition and presentation.
    pub fn gpu_timing_audit(&self) -> serde_json::Value {
        self.gpu_profiler.audit_value()
    }

    /// Render the frame
    pub fn render(&mut self) -> Result<()> {
        let flat_acquire_span =
            self.flat_span(ferrite_render::flat_reuse_diagnostics::FlatFrameStage::CompletionWait);
        self.validate_continuous_frame()?;
        let profiling = crate::profiler::is_profiling_enabled();

        // get_current_texture includes VSync wait — measure separately
        let get_tex_timer = if profiling {
            Some(ScopeTimer::new("get_texture"))
        } else {
            None
        };
        let output = self.state.get_current_texture()?;
        drop(flat_acquire_span);
        let flat_encode_span =
            self.flat_span(ferrite_render::flat_reuse_diagnostics::FlatFrameStage::EncodeAndSubmit);
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        if let Some(t) = get_tex_timer {
            self.cpu_profiler.record("get_texture", t.elapsed());
        }

        // render_total starts AFTER VSync wait for accurate CPU render cost
        let render_timer = if profiling {
            Some(ScopeTimer::new("render_total"))
        } else {
            None
        };

        let egui_timer = if profiling {
            Some(ScopeTimer::new("render_egui"))
        } else {
            None
        };
        let mut egui_output = self.prepare_overlay(true);
        self.prepare_chart_text(&mut egui_output);
        if let Some(t) = egui_timer {
            self.cpu_profiler.record("render_egui", t.elapsed());
        }

        let mut encoder =
            self.state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("render_encoder"),
                });

        let gpu_buf_timer = if profiling {
            Some(ScopeTimer::new("gpu_buffer_create"))
        } else {
            None
        };
        self.prepare_geometry_buffers();
        if let Some(t) = gpu_buf_timer {
            self.cpu_profiler.record("gpu_buffer_create", t.elapsed());
        }

        // Use MSAA texture as render target if available, resolve to surface
        let (target_view, resolve_target) = if let Some(ref msaa_view) = self.state.msaa_view {
            (msaa_view, Some(&view))
        } else {
            (&view, None)
        };

        let render_pass_timer = if profiling {
            Some(ScopeTimer::new("render_pass"))
        } else {
            None
        };
        // Pass timestamps need only TIMESTAMP_QUERY; encoder timestamps are optional.
        let chart_query = self.gpu_profiler.is_enabled().then(|| {
            self.gpu_profiler.profiler.begin_pass_query(
                "chart_pass",
                &mut encoder,
                &self.state.device,
            )
        });
        self.encode_chart(
            &mut encoder,
            target_view,
            resolve_target,
            chart_query.as_ref(),
        );
        if let Some(query) = chart_query {
            self.gpu_profiler.profiler.end_query(&mut encoder, query);
        }

        if let Some(t) = render_pass_timer {
            self.cpu_profiler.record("render_pass", t.elapsed());
        }

        // Render egui UI overlay (after chart rendering, to surface texture directly)
        let (width, height) = self.state.viewport_size();
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [width as u32, height as u32],
            pixels_per_point: self.state.window.scale_factor() as f32,
        };

        let egui_render_timer = if profiling {
            Some(ScopeTimer::new("egui_gpu_render"))
        } else {
            None
        };
        self.egui.render(
            &self.state.device,
            &self.state.queue,
            &mut encoder,
            &view,
            screen_descriptor,
            egui_output,
        );
        if let Some(t) = egui_render_timer {
            self.cpu_profiler.record("egui_gpu_render", t.elapsed());
        }

        // GPU profiler: resolve queries before submit
        if self.gpu_profiler.is_enabled() {
            self.gpu_profiler.profiler.resolve_queries(&mut encoder);
        }

        let submit_timer = if profiling {
            Some(ScopeTimer::new("queue_submit"))
        } else {
            None
        };
        self.state.queue.submit(std::iter::once(encoder.finish()));
        if let Some(t) = submit_timer {
            self.cpu_profiler.record("queue_submit", t.elapsed());
        }

        drop(flat_encode_span);
        // GPU profiler: end frame and process results
        if self.gpu_profiler.is_enabled() {
            if let Err(error) = self.gpu_profiler.profiler.end_frame() {
                tracing::warn!("GPU profiling frame rejected: {error}");
            }
            self.gpu_profiler.process_and_log(&self.state.queue);
        }

        let present_timer = if profiling {
            Some(ScopeTimer::new("present"))
        } else {
            None
        };
        let flat_present_span =
            self.flat_span(ferrite_render::flat_reuse_diagnostics::FlatFrameStage::PresentCall);
        output.present();
        drop(flat_present_span);
        if let Some(t) = present_timer {
            self.cpu_profiler.record("present", t.elapsed());
        }

        if let Some(t) = render_timer {
            self.cpu_profiler.record("render_total", t.elapsed());
        }

        Ok(())
    }

    /// Get window reference
    #[inline]
    /// Private hidden test control; production defaults OFF unless explicitly enabled.
    pub fn wait_hidden_key_frame(&self) -> Result<()> {
        if !crate::background_test::enabled()
            || self.window().is_visible().unwrap_or(true)
            || self.window().has_focus()
        {
            return Err(WgpuError::Render(
                "Key frame wait requires hidden unfocused mode".into(),
            ));
        }
        let _flat_wait_span =
            self.flat_span(ferrite_render::flat_reuse_diagnostics::FlatFrameStage::CompletionWait);
        self.state.device.poll(wgpu::Maintain::Wait);
        Ok(())
    }

    pub fn export_hidden_key_flat_coverage(
        &self,
        path: &std::path::Path,
        context: &RenderContext,
    ) -> Result<()> {
        if !crate::background_test::enabled()
            || self.window().is_visible().unwrap_or(true)
            || self.window().has_focus()
        {
            return Err(WgpuError::Render(
                "Flat coverage proof requires hidden unfocused window".into(),
            ));
        }
        std::fs::create_dir_all(path).map_err(|e| WgpuError::Render(e.to_string()))?;
        let ids: std::collections::BTreeSet<_> = context
            .raw_instructions()
            .iter()
            .filter_map(|i| i.cell_index())
            .collect();
        let mut passes = Vec::new();
        if let Some(prepared) = &self.prepared_coverage {
            prepared
                .validate(
                    context.geometry_revision(),
                    context.coverage_view_revision(),
                    context.instruction_count(),
                )
                .map_err(|e| WgpuError::Render(e.to_string()))?;
            for index in 0..prepared.pass_count() {
                let pass = prepared
                    .pass(index)
                    .map_err(|e| WgpuError::Render(e.to_string()))?;
                let decisions = (0..context.instruction_count())
                    .map(|i| pass.decision(i).map(|d| format!("{d:?}")))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| WgpuError::Render(e.to_string()))?;
                let mut masks = Vec::new();
                for id in &ids {
                    if let Some(mask) = pass.frame().mask(*id as usize) {
                        let file = format!("pass-{index}-dataset-{id}.r8");
                        std::fs::write(path.join(&file), mask.pixels())
                            .map_err(|e| WgpuError::Render(e.to_string()))?;
                        masks.push(serde_json::json!({"dataset":id,"origin":mask.origin(),"size":mask.size(),"file":file}));
                    } else {
                        masks.push(serde_json::json!({"dataset":id,"absent":true}));
                    }
                }
                passes.push(serde_json::json!({"pass":index,"decisions":decisions,"masks":masks}));
            }
        }
        std::fs::write(path.join("coverage.json"),serde_json::to_vec_pretty(&serde_json::json!({"bound":self.prepared_coverage.is_some(),"source_datasets":ids,"instruction_count":context.instruction_count(),"passes":passes})).map_err(|e|WgpuError::Render(e.to_string()))?).map_err(|e|WgpuError::Render(e.to_string()))?;
        Ok(())
    }

    pub fn window(&self) -> &Window {
        &self.state.window
    }

    /// Save screenshot to file
    pub fn save_screenshot<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        self.save_image(path, false)
    }

    /// Export the chart and application panels using the same overlay as the screen.
    /// Does not capture the desktop or another application.
    pub fn save_screenshot_with_ui<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        self.save_image(path, true)
    }

    fn save_image<P: AsRef<Path>>(&mut self, path: P, include_panels: bool) -> Result<()> {
        use crate::state::MSAA_SAMPLE_COUNT;

        let (width, height) = self.state.viewport_size();
        let width = width as u32;
        let height = height as u32;

        // Use the same format as the surface for pipeline compatibility
        let screenshot_format = self.state.format();

        // Create MSAA texture for rendering (only if MSAA is enabled)
        let msaa_texture = if MSAA_SAMPLE_COUNT > 1 {
            Some(self.state.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("screenshot_msaa_texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: MSAA_SAMPLE_COUNT,
                dimension: wgpu::TextureDimension::D2,
                format: screenshot_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            }))
        } else {
            None
        };
        let msaa_view = msaa_texture
            .as_ref()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));

        // Create resolve/output texture (non-MSAA, COPY_SRC for screenshot)
        let resolve_texture = self.state.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screenshot_resolve_texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: screenshot_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let resolve_view = resolve_texture.create_view(&wgpu::TextureViewDescriptor::default());

        tracing::debug!(
            "Screenshot format: {:?}, MSAA samples: {}",
            screenshot_format,
            MSAA_SAMPLE_COUNT
        );

        // Calculate buffer dimensions (aligned to 256 bytes)
        let bytes_per_pixel = 4u32;
        let unpadded_bytes_per_row = width * bytes_per_pixel;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let buffer_size = (padded_bytes_per_row * height) as u64;

        // Create output buffer
        let output_buffer = self.state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screenshot_buffer"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder =
            self.state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("screenshot_encoder"),
                });

        let mut overlay = self.prepare_overlay(include_panels);
        self.prepare_chart_text(&mut overlay);
        self.prepare_geometry_buffers();
        let (target_view, resolve_target) = if let Some(ref mv) = msaa_view {
            (mv, Some(&resolve_view))
        } else {
            (&resolve_view, None)
        };
        self.encode_chart(&mut encoder, target_view, resolve_target, None);

        // Screen and exported images share chart text and selection overlays.
        self.egui.render(
            &self.state.device,
            &self.state.queue,
            &mut encoder,
            &resolve_view,
            egui_wgpu::ScreenDescriptor {
                size_in_pixels: [width, height],
                pixels_per_point: self.state.window.scale_factor() as f32,
            },
            overlay,
        );

        // Copy resolved texture to buffer
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &resolve_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &output_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        self.state.queue.submit(std::iter::once(encoder.finish()));

        // Map buffer and read data
        let buffer_slice = output_buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        self.state.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|e| WgpuError::Render(format!("Failed to receive map result: {}", e)))?
            .map_err(|e| WgpuError::Render(format!("Buffer mapping failed: {:?}", e)))?;

        // Copy data and remove padding
        let data = buffer_slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height {
            let start = (row * padded_bytes_per_row) as usize;
            let end = start + (width * bytes_per_pixel) as usize;
            pixels.extend_from_slice(&data[start..end]);
        }
        drop(data);
        output_buffer.unmap();

        // Handle BGRA to RGBA conversion if needed
        let is_bgra = matches!(
            screenshot_format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        if is_bgra {
            // Swap B and R channels
            for chunk in pixels.as_chunks_mut::<4>().0 {
                chunk.swap(0, 2); // Swap B and R
            }
        }

        // Save as PNG
        let image = image::RgbaImage::from_raw(width, height, pixels)
            .ok_or_else(|| WgpuError::Render("Failed to create image from pixels".to_string()))?;
        image
            .save(path.as_ref())
            .map_err(|e| WgpuError::Render(format!("Failed to save screenshot: {}", e)))?;

        tracing::info!("Screenshot saved to: {}", path.as_ref().display());
        Ok(())
    }

    /// Get rendering statistics
    pub fn statistics(&self) -> RenderStats {
        RenderStats {
            area_vertices: self.area_vertices.len(),
            area_triangles: self.area_indices.len() / 3,
            line_vertices: self.line_vertices.len(),
            line_triangles: self.line_indices.len() / 3,
            symbol_instances: self.symbol_instances.len(),
            symbol_textures: self.symbol_textures.len(),
            text_labels: self.text_labels.len(),
        }
    }
}

/// Rendering statistics
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub area_vertices: usize,
    pub area_triangles: usize,
    pub line_vertices: usize,
    pub line_triangles: usize,
    pub symbol_instances: usize,
    pub symbol_textures: usize,
    pub text_labels: usize,
}

impl std::fmt::Display for RenderStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Areas: {} verts, {} tris | Lines: {} verts, {} tris | Symbols: {} instances, {} textures",
            self.area_vertices,
            self.area_triangles,
            self.line_vertices,
            self.line_triangles,
            self.symbol_instances,
            self.symbol_textures
        )
    }
}

/// Geographic AABB culling with a 50% animation margin and optional +/-360 copies.
fn longitude_bounds_relation(
    aabb: (f64, f64, f64, f64),
    bounds: Option<(f64, f64, f64, f64)>,
    wrapping: bool,
) -> ferrite_kernel::spatial_hierarchy::SpatialRelation {
    use ferrite_kernel::spatial_hierarchy::SpatialRelation as R;
    if !longitude_bounds_visible(aabb, bounds, wrapping) {
        return R::Outside;
    }
    let Some((x0, y0, x1, y1)) = bounds else {
        return R::Inside;
    };
    let (ax, ay, bx, by) = aabb;
    let mx = (x1 - x0) * 0.5;
    let my = (y1 - y0) * 0.5;
    let offsets = [0., -360., 360.];
    if ay >= y0 - my
        && by <= y1 + my
        && offsets[..if wrapping { 3 } else { 1 }]
            .iter()
            .any(|dx| ax + dx >= x0 - mx && bx + dx <= x1 + mx)
    {
        R::Inside
    } else {
        R::Intersecting
    }
}
fn longitude_bounds_visible(
    aabb: (f64, f64, f64, f64),
    bounds: Option<(f64, f64, f64, f64)>,
    wrapping: bool,
) -> bool {
    let Some((x0, y0, x1, y1)) = bounds else {
        return true;
    };
    let (ax, ay, bx, by) = aabb;
    let mx = (x1 - x0) * 0.5;
    let my = (y1 - y0) * 0.5;
    if by < y0 - my || ay > y1 + my {
        return false;
    }
    let offsets = [0., -360., 360.];
    offsets[..if wrapping { 3 } else { 1 }]
        .iter()
        .any(|offset| bx + offset >= x0 - mx && ax + offset <= x1 + mx)
}
#[cfg(test)]
mod longitude_culling_tests {
    use super::*;
    #[test]
    fn culls_against_the_actual_enabled_copies_with_animation_margin() {
        let view = Some((350., 0., 370., 10.));
        assert!(!longitude_bounds_visible((0., 5., 0., 5.), view, false));
        assert!(longitude_bounds_visible((0., 5., 0., 5.), view, true));
        assert!(longitude_bounds_visible((720., 5., 720., 5.), view, true));
        assert!(!longitude_bounds_visible(
            (-360., 5., -360., 5.),
            view,
            true
        ));
        assert!(!longitude_bounds_visible((0., 50., 0., 50.), view, true));
        assert!(longitude_bounds_visible((-5., 0., 5., 10.), view, true));
        assert!(longitude_bounds_visible((338., 5., 342., 5.), view, false));
        assert!(!longitude_bounds_visible((325., 5., 329., 5.), view, true));
    }
}

/// Intersect a chart's physical-pixel viewport with the current render target.
fn chart_pass_scissor(viewport: ferrite_render::Viewport, extent: [u32; 2]) -> Option<[u32; 4]> {
    let [vx, vy, vw, vh] = [viewport.x, viewport.y, viewport.width, viewport.height].map(f64::from);
    if ![vx, vy, vw, vh].iter().all(|v| v.is_finite())
        || vw <= 0.
        || vh <= 0.
        || extent.contains(&0)
    {
        return None;
    }
    let x = vx.floor().clamp(0., extent[0] as f64) as u32;
    let y = vy.floor().clamp(0., extent[1] as f64) as u32;
    let right = (vx + vw).ceil().clamp(0., extent[0] as f64) as u32;
    let bottom = (vy + vh).ceil().clamp(0., extent[1] as f64) as u32;
    (right > x && bottom > y).then(|| [x, y, right - x, bottom - y])
}
fn intersect_scissors(a: [u32; 4], b: [u32; 4]) -> Option<[u32; 4]> {
    let x = a[0].max(b[0]);
    let y = a[1].max(b[1]);
    let right = a[0].saturating_add(a[2]).min(b[0].saturating_add(b[2]));
    let bottom = a[1].saturating_add(a[3]).min(b[1].saturating_add(b[3]));
    (right > x && bottom > y).then(|| [x, y, right - x, bottom - y])
}
#[cfg(test)]
mod chart_pass_scissor_tests {
    use super::*;
    #[test]
    fn shifted_resized_and_invalid_viewports_are_bounded() {
        use ferrite_render::Viewport as V;
        assert_eq!(
            chart_pass_scissor(V::with_origin(160., 120., 480., 320.), [960, 640]),
            Some([160, 120, 480, 320])
        );
        assert_eq!(
            chart_pass_scissor(V::with_origin(-10., 20., 100., 80.), [50, 60]),
            Some([0, 20, 50, 40])
        );
        assert_eq!(
            chart_pass_scissor(V::with_origin(1.2, 2.2, 3.2, 4.2), [50, 60]),
            Some([1, 2, 4, 5])
        );
        for v in [
            V::new(0., 10.),
            V::new(f32::NAN, 10.),
            V::with_origin(f32::INFINITY, 0., 10., 10.),
            V::with_origin(100., 100., 10., 10.),
        ] {
            assert_eq!(chart_pass_scissor(v, [50, 60]), None);
        }
        assert_eq!(chart_pass_scissor(V::new(20., 20.), [0, 60]), None);
    }
    #[test]
    fn text_scissor_never_expands_the_chart_pane() {
        assert_eq!(
            intersect_scissors([0, 0, 960, 640], [160, 120, 480, 320]),
            Some([160, 120, 480, 320])
        );
        assert_eq!(
            intersect_scissors([600, 400, 100, 100], [160, 120, 480, 320]),
            Some([600, 400, 40, 40])
        );
        assert_eq!(
            intersect_scissors([0, 0, 10, 10], [160, 120, 480, 320]),
            None
        );
    }
}

#[cfg(test)]
mod raster_publication_stage_tests {
    use super::*;
    #[test]
    fn renderer_lifetime_and_source_inventory_epoch_must_both_match() {
        let owner = Arc::new(());
        let epoch = Arc::new(());
        assert!(raster_publication_identity_matches(
            &owner, &owner, &epoch, &epoch
        ));
        assert!(!raster_publication_identity_matches(
            &owner,
            &Arc::new(()),
            &epoch,
            &epoch
        ));
        assert!(!raster_publication_identity_matches(
            &owner,
            &owner,
            &epoch,
            &Arc::new(())
        ));
    }
}

#[cfg(test)]
mod pattern_emission_audit_tests {
    use super::*;
    fn record(source: usize) -> PatternEmissionAudit {
        PatternEmissionAudit {
            source_ordinal: source,
            vertex_start: 7199,
            vertex_end: 7204,
            index_start: 0,
            index_end: 3,
            wrap_mode: 255,
            wrap_dx_screen_bits: 0_f64.to_bits(),
        }
    }
    #[test]
    fn owned_vertices_include_unused_earcut_prefix_and_exact_source() {
        let mut records = Vec::new();
        let mut dropped = 0;
        record_pattern_emission(&mut records, &mut dropped, record(66));
        let r = &records[0];
        assert_eq!(
            (r.source_ordinal, r.vertex_start, r.vertex_end),
            (66, 7199, 7204)
        );
        assert_eq!(
            (
                r.index_start,
                r.index_end,
                r.wrap_mode,
                r.wrap_dx_screen_bits
            ),
            (0, 3, 255, 0_f64.to_bits())
        );
        assert_eq!(dropped, 0);
    }
    #[test]
    fn bounded_audit_reports_incompleteness_instead_of_silent_subset() {
        let mut records = vec![record(0); MAX_PATTERN_AUDIT_EMISSIONS];
        let mut dropped = 0;
        record_pattern_emission(&mut records, &mut dropped, record(1));
        assert_eq!(records.len(), MAX_PATTERN_AUDIT_EMISSIONS);
        assert_eq!(dropped, 1);
    }
}
