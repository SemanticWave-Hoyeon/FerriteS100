//! Main wgpu Renderer
//!
//! Orchestrates rendering of drawing instructions to the screen.
//! Uses resvg for SVG symbol rendering via textures.

#[path = "immutable_text_preparation.rs"]
mod immutable_text_preparation;

// Opt-in runtime diagnostics. A frame owns one collector; no diagnostic clocks or
// instruction guard allocations are created when the collector is absent.
type FlatDiagnosticCell =
    crate::shared_cell::Shared<ferrite_render::flat_reuse_diagnostics::FlatFrameSample>;
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
    resource_owner: u64,
    screen_x: f32,
    screen_y: f32,
    scale: f32,
    rotation: f32,
}

/// Pending text label to render via egui painter overlay
struct TextLabel {
    referenced_font: Option<ferrite_portrayal_catalog::BoundFontReference>,
    font_family_override: Option<egui::FontFamily>,
    font_style: ferrite_render::TextFontStyle,
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
#[derive(Clone)]
struct GpuChartText {
    texture_id: egui::TextureId,
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
/// Bound producer output and explicitly unbound IC compatibility output are
/// separate variants. Legacy output never acquires a producer certificate.
#[cfg(feature = "s102-portrayal")]
pub enum S102RasterMaterial {
    Bound(ferrite_s102::BoundBathymetryMaterial),
    LegacyIc(ferrite_render::RasterLayer),
}
#[cfg(feature = "s102-portrayal")]
impl S102RasterMaterial {
    pub fn bounds(&self) -> ferrite_render::GeoBounds {
        match self {
            Self::Bound(v) => v.bounds(),
            Self::LegacyIc(v) => v.bounds,
        }
    }
}

#[derive(Clone)]
struct GpuRasterLayer {
    #[cfg(feature = "s102-portrayal")]
    regular_portrayal_owner: Option<Arc<ferrite_s102::BoundBathymetryEvaluation>>,
    // Actual scaler identity used to create THIS geometry; not an old-view stamp.
    prepared_camera: Option<[u64; 16]>,
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
    enabled_groups: Option<std::collections::HashSet<u32>>,
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

#[derive(Default)]
struct VectorGeometryCpu {
    owner: std::sync::Arc<()>,
    // World-map CPU buffers share chart frame ownership for private scene staging.
    world_map_line_vertices: Vec<LineVertex>,
    world_map_line_indices: Vec<u32>,
    world_map_mask_vertices: Vec<Vertex2D>,
    world_map_mask_indices: Vec<u32>,
    area_vertices: Vec<Vertex2D>,
    area_indices: Vec<u32>,
    line_geometry: crate::primary_line_geometry::Geometry,
    symbol_instances: Vec<SymbolInstance>,
    pattern_vertices: Vec<PatternVertex>,
    pattern_indices: Vec<u32>,
    text_labels: Vec<TextLabel>,
    area_priority_ranges: Vec<(CompositionPlane, i32, usize, usize, Option<usize>)>,
    line_priority_ranges: Vec<(CompositionPlane, i32, usize, usize, Option<usize>)>,
    symbol_priority_ranges: Vec<(CompositionPlane, i32, usize, usize, Option<usize>)>,
    pattern_ranges: Vec<PatternDrawRange>,
    displayed_geometry: Vec<usize>,
    pattern_emission_audit: Option<Vec<PatternEmissionAudit>>,
    pattern_emission_audit_dropped: usize,
}
struct VectorGeometryPrefix {
    owner: std::sync::Arc<()>,
    lengths: [usize; 13],
    line_prefix: crate::primary_line_geometry::Prefix,
    audit_len: Option<usize>,
    audit_dropped: usize,
}
impl VectorGeometryCpu {
    fn lengths(&self) -> [usize; 13] {
        [
            self.area_vertices.len(),
            self.area_indices.len(),
            self.line_geometry.vertex_len(),
            self.line_geometry.index_len(),
            self.symbol_instances.len(),
            self.pattern_vertices.len(),
            self.pattern_indices.len(),
            self.text_labels.len(),
            self.area_priority_ranges.len(),
            self.line_priority_ranges.len(),
            self.symbol_priority_ranges.len(),
            self.pattern_ranges.len(),
            self.displayed_geometry.len(),
        ]
    }
    fn prefix(&self) -> VectorGeometryPrefix {
        VectorGeometryPrefix {
            owner: std::sync::Arc::clone(&self.owner),
            lengths: self.lengths(),
            line_prefix: self.line_geometry.prefix(),
            audit_len: self.pattern_emission_audit.as_ref().map(Vec::len),
            audit_dropped: self.pattern_emission_audit_dropped,
        }
    }
    // Preserve original dependency-trial prefix exactly; reject foreign/stale
    // prefixes BEFORE any truncate. No changing permission or collision rules.
    fn truncate_trial(&mut self, prefix: &VectorGeometryPrefix) -> crate::Result<()> {
        if !std::sync::Arc::ptr_eq(&self.owner, &prefix.owner)
            || self
                .lengths()
                .iter()
                .zip(prefix.lengths)
                .any(|(now, old)| *now < old)
            || match (&self.pattern_emission_audit, prefix.audit_len) {
                (None, None) => false,
                (Some(records), Some(len)) => records.len() < len,
                _ => true,
            }
        {
            return Err(crate::WgpuError::Render(
                "Foreign or stale private geometry prefix".into(),
            ));
        }
        self.line_geometry
            .validate_prefix(&prefix.line_prefix)
            .map_err(|e| crate::WgpuError::Render(e.into()))?;
        self.line_geometry
            .truncate(&prefix.line_prefix)
            .map_err(|e| crate::WgpuError::Render(e.into()))?;
        self.area_vertices.truncate(prefix.lengths[0]);
        self.area_indices.truncate(prefix.lengths[1]);

        self.symbol_instances.truncate(prefix.lengths[4]);
        self.pattern_vertices.truncate(prefix.lengths[5]);
        self.pattern_indices.truncate(prefix.lengths[6]);
        self.text_labels.truncate(prefix.lengths[7]);
        self.area_priority_ranges.truncate(prefix.lengths[8]);
        self.line_priority_ranges.truncate(prefix.lengths[9]);
        self.symbol_priority_ranges.truncate(prefix.lengths[10]);
        self.pattern_ranges.truncate(prefix.lengths[11]);
        self.displayed_geometry.truncate(prefix.lengths[12]);
        if let (Some(records), Some(len)) = (&mut self.pattern_emission_audit, prefix.audit_len) {
            records.truncate(len);
        }
        self.pattern_emission_audit_dropped = prefix.audit_dropped;
        Ok(())
    }
}
#[cfg(test)]
mod vector_geometry_cpu_tests {
    use super::*;
    #[test]
    fn private_trial_preserves_all_overlay_prefixes_and_resets_only_its_tail() {
        let mut cpu = VectorGeometryCpu::default();
        cpu.area_vertices
            .push(Vertex2D::new(2., 3., [0.1, 0.2, 0.3, 1.]));
        cpu.area_indices.extend([0, 0, 0]);
        cpu.line_geometry
            .legacy_indices_mut()
            .unwrap()
            .extend([1, 2]);
        cpu.pattern_indices.extend([3, 4]);
        cpu.displayed_geometry.extend([7, 9]);
        cpu.pattern_emission_audit = Some(Vec::new());
        cpu.pattern_emission_audit_dropped = 3;
        let prefix = cpu.prefix();
        cpu.area_vertices.push(Vertex2D::new(10., 20., [1.; 4]));
        cpu.area_indices.push(99);
        cpu.line_geometry.legacy_indices_mut().unwrap().push(100);
        cpu.pattern_indices.push(101);
        cpu.displayed_geometry.push(102);
        cpu.pattern_emission_audit_dropped = 50;
        cpu.truncate_trial(&prefix).unwrap();
        assert_eq!(cpu.lengths(), prefix.lengths);
        assert_eq!(cpu.area_vertices[0].position, [2., 3.]);
        assert_eq!(cpu.area_vertices[0].color, [0.1, 0.2, 0.3, 1.]);
        assert_eq!(cpu.displayed_geometry, [7, 9]);
        assert_eq!(cpu.pattern_emission_audit_dropped, 3);
    }
    #[test]
    fn packed_trial_retains_exact_ranges_and_original_export_after_degrade() {
        let mut cpu = VectorGeometryCpu::default();
        cpu.line_geometry.begin_frame(true);
        cpu.line_geometry
            .append_emitted([0., -0.], [10., 20.], [-0., -1.], [0., 1.], [1.; 4])
            .unwrap();
        cpu.line_priority_ranges.push((
            CompositionPlane::new(
                CompositionStage::Chart,
                std::num::NonZeroI32::new(1).unwrap(),
            ),
            1,
            0,
            6,
            None,
        ));
        let prefix = cpu.prefix();
        cpu.line_geometry
            .append_emitted([5., 1.], [20., 30.], [-0., -1.], [0., 1.], [1.; 4])
            .unwrap();
        cpu.line_priority_ranges.push((
            CompositionPlane::new(
                CompositionStage::Chart,
                std::num::NonZeroI32::new(1).unwrap(),
            ),
            2,
            6,
            12,
            None,
        ));
        cpu.line_geometry.materialize().unwrap();
        cpu.truncate_trial(&prefix).unwrap();
        assert_eq!(cpu.line_geometry.index_len(), 6);
        assert_eq!(cpu.line_priority_ranges.len(), 1);
        assert_eq!(
            cpu.line_geometry.export_legacy().unwrap().1,
            [0, 1, 2, 0, 2, 3]
        );
    }
    #[test]
    fn same_cpu_new_frame_stale_line_prefix_does_not_truncate_area() {
        let mut cpu = VectorGeometryCpu::default();
        cpu.line_geometry.begin_frame(true);
        cpu.area_indices.push(3);
        let prefix = cpu.prefix();
        cpu.line_geometry.begin_frame(true);
        cpu.area_indices.push(4);
        assert!(cpu.truncate_trial(&prefix).is_err());
        assert_eq!(cpu.area_indices, [3, 4]);
    }
    #[test]
    fn foreign_or_stale_prefix_cannot_partially_truncate_outputs() {
        let mut a = VectorGeometryCpu::default();
        a.area_indices.extend([1, 2]);
        let foreign = VectorGeometryCpu::default().prefix();
        assert!(a.truncate_trial(&foreign).is_err());
        assert_eq!(a.area_indices, [1, 2]);
        let prefix = a.prefix();
        a.area_indices.pop();
        a.line_geometry.legacy_indices_mut().unwrap().extend([8, 9]);
        assert!(a.truncate_trial(&prefix).is_err());
        assert_eq!(a.area_indices, [1]);
        assert_eq!(a.line_geometry.legacy().unwrap().1, [8, 9]);
    }
}

// Immutable CPU text inputs, with no renderer/window/GPU or live UI reference.
// Candidate preparation must own the source classification/set borrowed here.
struct ChartTextLayoutEnvironment<'a> {
    physical_extent: [f32; 2],
    pan: [f32; 2],
    zoom: [f32; 2],
    pivot: [f32; 2],
    longitude_wrap: f32,
    source_classification: Option<&'a ferrite_render::StaticSourceClassification>,
    device_fixed_sources: &'a FxHashSet<usize>,
}
impl ChartTextLayoutEnvironment<'_> {
    fn is_device_fixed(&self, ordinal: usize) -> bool {
        self.source_classification.map_or_else(
            || self.device_fixed_sources.contains(&ordinal),
            |classification| classification.is_device_fixed(ordinal),
        )
    }
}

fn layout_chart_text_with_fonts(
    environment: ChartTextLayoutEnvironment<'_>,
    chart_fonts: &egui::Context,
    text_labels: &[TextLabel],
    mut shapes: Vec<ChartTextShape>,
    capture_shapes: bool,
) -> (Vec<ChartTextShape>, Vec<bool>) {
    shapes.clear();
    let mut accepted = if capture_shapes {
        Vec::new()
    } else {
        vec![false; text_labels.len()]
    };
    let [width, height] = environment.physical_extent;
    let ppp = chart_fonts.pixels_per_point();
    let clip = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width / ppp, height / ppp));
    // Apply the same GPU pan/zoom offset so text tracks with chart geometry during drag
    if !text_labels.is_empty() {
        let pan_x = environment.pan[0];
        let pan_y = environment.pan[1];
        let zoom = environment.zoom[0];
        let (pivot_x, pivot_y) = (environment.pivot[0], environment.pivot[1]);

        let painter = chart_fonts.layer_painter(egui::LayerId::background());
        let mut placement = ferrite_render::TextPlacement::default();
        let mut labels: Vec<_> = (0..text_labels.len()).collect();
        // Higher planes/priorities retain text on collisions; stable source
        // order provides a neutral tie break. This is a display-engine policy.
        labels.sort_by_key(|&index| {
            std::cmp::Reverse((text_labels[index].plane, text_labels[index].priority))
        });
        for label_index in labels {
            let label = &text_labels[label_index];
            let color = egui::Color32::from_rgba_unmultiplied(
                (label.color[0] * 255.0) as u8,
                (label.color[1] * 255.0) as u8,
                (label.color[2] * 255.0) as u8,
                (label.color[3] * 255.0) as u8,
            );

            let (family, synthetic_italic) = if let Some(family) = &label.font_family_override {
                // Explicit captured PC reference never enters best-match or synthetic shear.
                (family.clone(), false)
            } else {
                let matching = crate::chart_fonts::match_chart_characteristics(
                    &label.font_style,
                    label.bold,
                    label.italic,
                );
                (matching.family(), matching.synthetic_italic)
            };
            // Build a LayoutJob to support bold/italic font variants
            let mut job = egui::text::LayoutJob::single_section(
                label.text.clone(),
                egui::TextFormat {
                    font_id: egui::FontId {
                        size: label.font_size / chart_fonts.pixels_per_point(),
                        family,
                    },
                    color,
                    // Existing receiver synthetic slant is preserved. Matching
                    // reports the actual upright-face mismatch separately.
                    italics: synthetic_italic,
                    underline: if label.font_style.underline && color.a() > 0 {
                        egui::Stroke::new(1_f32, color)
                    } else {
                        egui::Stroke::NONE
                    },
                    strikethrough: if label.font_style.strikethrough && color.a() > 0 {
                        egui::Stroke::new(1_f32, color)
                    } else {
                        egui::Stroke::NONE
                    },
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

            let wrap_offsets = [0.0, -environment.longitude_wrap, environment.longitude_wrap];
            let copies = if environment.longitude_wrap > 0.0
                && !label.source.is_some_and(|s| environment.is_device_fixed(s))
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
                    / chart_fonts.pixels_per_point();
                let sy = ((sy - pivot_y) * environment.zoom[1] + pivot_y + label.screen_y
                    - label.anchor[1])
                    / chart_fonts.pixels_per_point();

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
                let mut bounds = if label.background.is_some() {
                    galley.rect.union(galley.mesh_bounds)
                } else {
                    galley.mesh_bounds
                };
                if label.font_style.upperline && color.a() > 0 {
                    bounds = bounds.union(egui::Rect::from_min_max(
                        galley.rect.min - egui::vec2(0.5, 0.5),
                        egui::pos2(galley.rect.max.x + 0.5, galley.rect.min.y + 0.5),
                    ));
                }
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
                    if label.font_style.upperline && color.a() > 0 {
                        let rotate = |p: egui::Pos2| {
                            egui::pos2(
                                origin.x + p.x * angle.cos() - p.y * angle.sin(),
                                origin.y + p.x * angle.sin() + p.y * angle.cos(),
                            )
                        };
                        shapes.push((
                            label.plane,
                            label.priority,
                            label.source,
                            coverage_wrap_pass,
                            egui::epaint::ClippedShape {
                                clip_rect: clip,
                                shape: egui::Shape::line_segment(
                                    [
                                        rotate(galley.rect.min),
                                        rotate(egui::pos2(galley.rect.max.x, galley.rect.min.y)),
                                    ],
                                    egui::Stroke::new(1_f32, color),
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

#[cfg(test)]
mod detached_text_layout_tests {
    use super::*;
    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::chart_fonts::chart_font_definitions());
        ctx.begin_pass(egui::RawInput::default());
        ctx
    }
    fn label(source: usize, priority: i32, position: [f32; 2]) -> TextLabel {
        TextLabel {
            referenced_font: None,
            font_family_override: None,
            font_style: Default::default(),
            source: Some(source),
            plane: CompositionPlane::new(
                CompositionStage::Chart,
                std::num::NonZeroI32::new(1).unwrap(),
            ),
            priority,
            anchor: position,
            rotation: 0.,
            screen_x: position[0],
            screen_y: position[1],
            text: "Depth 12.3".into(),
            font_size: 16.,
            color: [1.; 4],
            background: None,
            bold: true,
            italic: true,
            h_align: ferrite_render::HAlign::Left,
            v_align: ferrite_render::VAlign::Top,
        }
    }
    fn environment(fixed: &FxHashSet<usize>, wrap: f32) -> ChartTextLayoutEnvironment<'_> {
        ChartTextLayoutEnvironment {
            physical_extent: [1200., 600.],
            pan: [0.; 2],
            zoom: [1.; 2],
            pivot: [0.; 2],
            longitude_wrap: wrap,
            source_classification: None,
            device_fixed_sources: fixed,
        }
    }
    #[test]
    fn independent_font_owner_keeps_parent_collision_mask_and_priority() {
        let live = context();
        let private = context();
        let _ = live
            .layer_painter(egui::LayerId::background())
            .layout_no_wrap(
                "Warm UI cache different atlas packing".into(),
                egui::FontId::proportional(13.),
                egui::Color32::WHITE,
            );
        let labels = [
            label(0, 1, [100., 100.]),
            label(1, 2, [100., 100.]),
            label(2, 3, [5000., 5000.]),
        ];
        let fixed = FxHashSet::default();
        let a = layout_chart_text_with_fonts(
            environment(&fixed, 0.),
            &live,
            &labels,
            Vec::new(),
            false,
        )
        .1;
        let b = layout_chart_text_with_fonts(
            environment(&fixed, 0.),
            &private,
            &labels,
            Vec::new(),
            false,
        )
        .1;
        assert_eq!(a, [false, true, false]);
        assert_eq!(b, a);
        let _ = live.end_pass();
        let _ = private.end_pass();
    }
    #[test]
    fn decorated_text_retains_glyph_styles_and_rotates_upperline_with_owner() {
        let ctx = context();
        let fixed = FxHashSet::default();
        let mut text = label(3, 4, [100., 100.]);
        text.rotation = 30.;
        text.font_style.proportion = ferrite_render::TextFontProportion::MonoSpaced;
        text.font_style.underline = true;
        text.font_style.strikethrough = true;
        text.font_style.upperline = true;
        let shapes =
            layout_chart_text_with_fonts(environment(&fixed, 0.), &ctx, &[text], Vec::new(), true)
                .0;
        assert_eq!(shapes.len(), 2);
        assert!(shapes.iter().all(|s| s.2 == Some(3) && s.1 == 4));
        let egui::Shape::LineSegment { points, stroke } = &shapes[0].4.shape else {
            panic!("upperline")
        };
        assert!(stroke.width > 0.);
        let delta = points[1] - points[0];
        assert!(delta.x > 0. && delta.y > 0.);
        assert!((delta.y / delta.x - 30_f32.to_radians().tan()).abs() < 1e-5);
        let egui::Shape::Text(text) = &shapes[1].4.shape else {
            panic!("glyphs")
        };
        let format = &text.galley.job.sections[0].format;
        assert_eq!(format.font_id.family, egui::FontFamily::Monospace);
        assert!(format.underline.width > 0. && format.strikethrough.width > 0.);
        let _ = ctx.end_pass();
    }
    #[test]
    fn invisible_decorations_do_not_expand_background_only_collision_bounds() {
        let ctx = context();
        let fixed = FxHashSet::default();
        let mut a = label(0, 1, [100., 100.]);
        a.color = [0.; 4];
        a.background = Some([1.; 4]);
        let baseline =
            layout_chart_text_with_fonts(environment(&fixed, 0.), &ctx, &[a], Vec::new(), true).0;
        let mut b = label(0, 1, [100., 100.]);
        b.color = [0.; 4];
        b.background = Some([1.; 4]);
        b.font_style.underline = true;
        b.font_style.strikethrough = true;
        b.font_style.upperline = true;
        let decorated =
            layout_chart_text_with_fonts(environment(&fixed, 0.), &ctx, &[b], Vec::new(), true).0;
        assert_eq!(baseline.len(), decorated.len());
        assert_eq!(baseline[0].4.shape, decorated[0].4.shape);
        let _ = ctx.end_pass();
    }
    #[test]
    fn detached_environment_preserves_device_fixed_and_world_wrap_ownership() {
        let ctx = context();
        let mut fixed = FxHashSet::default();
        fixed.insert(1);
        let labels = [label(1, 1, [100., 100.]), label(2, 1, [100., 200.])];
        let shapes = layout_chart_text_with_fonts(
            environment(&fixed, 700.),
            &ctx,
            &labels,
            Vec::new(),
            true,
        )
        .0;
        assert_eq!(shapes.iter().filter(|s| s.2 == Some(1)).count(), 1);
        assert_eq!(shapes.iter().filter(|s| s.2 == Some(2)).count(), 2);
        assert!(shapes.iter().filter(|s| s.2 == Some(1)).all(|s| s.3 == 0));
        let _ = ctx.end_pass();
    }
}

/// Authoritative vector-frame ownership grouping only. No private preparation,
/// validation token, atomic swap or font/GPU alias isolation is implemented here.
/// Mutable caches, UI, raster proofs and GPU/upload resources stay on Renderer.
struct VectorFrameState {
    /// Owned CPU chart outputs; GPU assets and font state are still separate.
    frame_cpu: VectorGeometryCpu,
    geometry_transform: Option<ferrite_render::FlatTransform>,
    /// Collected area vertices
    /// Collected line vertices
    temporal_visibility_counts: (usize, usize),
    temporal_visibility_mask: Vec<bool>,
    display_scale: u32,
    coverage_scale_colour: Option<egui::Color32>,
    coverage_scale_colours: std::collections::BTreeMap<usize, egui::Color32>,
    /// Screen-space pan offset (pixels) for fast panning during drag
    screen_pan_offset: (f32, f32),
    selection_anchor: Option<[f32; 2]>,
    selection_world_geometry: Vec<Vec<WorldPoint>>,
    selection_screen_geometry: Vec<Vec<[f32; 2]>>,
    selection_index: std::cell::OnceCell<ferrite_render::SelectionIndex>,
    dependency_status: DependencyRenderStatus,
    /// GPU zoom scale for smooth zooming (1.0 = no zoom delta, rebuilt at this level)
    screen_zoom_scale: f32,
    screen_zoom_scale_y: f32,
    /// Zoom pivot point in screen coordinates
    screen_zoom_pivot: (f32, f32),
    view_dependent_symbols: bool,
    view_clipped_patterns: bool,
    /// Retained-pixel AABB of every wrapped chart primitive anchor, computed
    /// once per scene. None = unknown: every longitude copy is encoded.
    scene_bounds: Option<[f32; 4]>,
    /// LOD level (0=full detail, 1=medium, 2=low)
    /// Viewport bounds in world coordinates for culling
    viewport_world_bounds: Option<(f64, f64, f64, f64)>,
    prepared_coverage: Option<Arc<ferrite_render::PreparedCoverage>>,
    overscale_annotation: Vec<crate::overscale_annotation::OverscaleAnnotation>,
    coverage_frame: Option<crate::coverage_gpu_frame::CoverageGpuFrame>,
    coverage_failed: bool,
    emitting_coverage_source: Option<usize>,
    device_fixed_sources: FxHashSet<usize>,
    static_source_classification:
        Option<std::sync::Arc<ferrite_render::StaticSourceClassification>>,
    /// Geometry viewport shared by native and exported chart passes.
    chart_geometry_viewport: Option<ferrite_render::Viewport>,
    /// Bounding boxes of loaded chart cells (world coords).
    /// Used to draw opaque background rectangles that mask world map under charts.
    world_map_chart_boxes: Vec<(f64, f64, f64, f64)>,
    /// Screen pixels corresponding to 360° of longitude (0 = wrapping disabled)
    lon_wrap_screen_px: f32,
}

impl VectorFrameState {
    /// Bounds of what the longitude-wrapped passes draw, in retained pixels.
    /// Requires legacy (materialized) lines; otherwise stays unknown.
    fn compute_scene_bounds(&mut self) {
        let cpu = &self.frame_cpu;
        let Some((lines, _)) = cpu.line_geometry.legacy() else {
            self.scene_bounds = None;
            return;
        };
        let points = cpu
            .area_vertices
            .iter()
            .map(|v| v.position)
            .chain(lines.iter().map(|v| v.position))
            .chain(cpu.pattern_vertices.iter().map(|v| v.position))
            .chain(cpu.symbol_instances.iter().map(|s| s.anchor));
        let mut b = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for p in points {
            // A non-finite coordinate makes the bounds unknown, never smaller.
            if !(p[0].is_finite() && p[1].is_finite()) {
                self.scene_bounds = None;
                return;
            }
            b = [
                b[0].min(p[0]),
                b[1].min(p[1]),
                b[2].max(p[0]),
                b[3].max(p[1]),
            ];
        }
        self.scene_bounds = Some(b);
    }

    fn continuous_transform_key(&self, viewport_size: (f32, f32)) -> [u32; 9] {
        let (width, height) = viewport_size;
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
}

pub struct WgpuRenderer {
    vector_emission: VectorEmissionOwned,

    draw_range_index_enabled: bool,

    raster_layers: Vec<GpuRasterLayer>,
    continuous_owner: std::sync::Arc<()>,
    raster_epoch: Arc<()>,
    vector_scene_epoch: private_vector_gpu::VectorSceneEpoch,
    continuous_uniforms: [Option<ViewUniforms>; 3],
    continuous_frame: Option<crate::ValidatedContinuousFrame>,
    // Updated only on scene commit: regular frames avoid scanning raster materials.
    continuous_layer_count: usize,
    raster_enabled_groups: Option<std::collections::HashSet<u32>>,
    pub state: GpuState,
    pub pipelines: RenderPipelines,
    pub view_buffer: wgpu::Buffer,
    pub view_bind_group: wgpu::BindGroup,

    /// GPU texture cache for symbols (keyed by interned SymbolId for cache efficiency)
    /// Symbol instances to render
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

    /// Chart compilation scale (e.g., 22000 for 1:22000)
    /// Used to calculate viewing scale for S-101 feature filtering
    pub compilation_scale: u32,
    /// Cached symbol classification flags (computed once per unique SymbolId, never cleared)
    /// Set of SymbolIds that could not be rendered (missing SVG, color profile, etc).
    /// Used to log each missing symbol exactly once instead of every frame, and to
    /// surface a "N symbols missing" count to the debug HUD/logs.
    /// Count of point instructions that arrived with an empty symbol_ref. Indicates a
    /// portrayal-rules bug (Lua emitted a Point without a symbol). Tracked but not
    /// rendered — silently swallowing this would hide chart-data quality issues.
    empty_selection_stats: ferrite_render::SelectionIndexStats,

    /// egui integration for UI overlay
    egui: EguiIntegration,
    /// UI state shared with main app
    pub ui_state: AppUiState,
    // === OPTIMIZATION FIELDS ===
    /// Geometry allocations are unique only within an unchanged instruction lifetime.
    /// Batched symbols by texture (optimization, keyed by interned SymbolId)
    /// Packed symbol vertices for single-buffer rendering
    packed_symbol_vertices: Vec<TextureVertex>,
    /// Packed symbol indices for single-buffer rendering
    packed_symbol_indices: Vec<u32>,
    /// Ranges into packed arrays per symbol texture: (symbol_id, index_start, index_count)
    packed_symbol_ranges: Vec<((u64, SymbolId), u32, u32)>,
    /// Animation/drag mode - enables fast-path rendering
    pub animation_mode: bool,
    /// Sampled once (`FERRITE_MOTION_PREVIEW`): approximate affine navigation.
    motion_preview: bool,
    /// The visible affine currently shows an approximate preview, not a
    /// consumer scene. Cleared by the next rebuild.
    motion_preview_active: bool,
    /// Second warm emission owner for background scene builds; swapped with
    /// the displayed one on install.
    idle_builder: Option<VectorEmissionOwned>,
    /// Bumped by any change a background build could not observe; a built
    /// scene from an older generation is discarded.
    scene_generation: u64,
    /// A background build is in flight: preview beyond the drift budget.
    scene_build_pending: bool,
    /// The accepted preview passed `PREFETCH_FRACTION` of its drift budget.
    preview_refresh_due: bool,
    /// Independent chart-font metrics for background builds. Sharing the live
    /// UI context would make every UI pass wait on the worker's layout lock.
    worker_fonts: Option<(PrivateEmissionEnvironment, egui::Context)>,

    // Sampled once: coarse OFF changes observers only, not rendering or collectors.
    overscale_program_reuse: Arc<crate::overscale_annotation::ProgramReuse>,

    native_route_gpu_owner: Arc<crate::NativeRouteGpuOwner>,
    native_route_gpu: Option<crate::PreparedNativeRouteGpuPublication>,
    native_route_last_encoded: std::cell::Cell<bool>,

    // === S-101 PRIORITY GROUP RENDERING ===
    /// Area index ranges by priority: (display_plane, priority, start_index, end_index)
    /// Line index ranges by priority: (display_plane, priority, start_index, end_index)
    /// Symbol instance ranges by priority: (display_plane, priority, start_index, end_index)
    // === PATTERN FILL (S-100 GPU texture-repeat tiling) ===
    /// Pattern fill vertices (TextureVertex: position + inv_tile_size)
    /// Pattern fill indices
    /// Pattern fill ranges: (display_plane, priority, index_start, index_end, pattern_texture_key)
    /// Pattern fill GPU textures (keyed by "{symbol}_pat")
    /// Pending text labels to render via egui painter
    chart_text_shapes: Vec<(
        CompositionPlane,
        i32,
        Option<usize>,
        u8,
        egui::epaint::ClippedShape,
    )>,
    chart_text_meshes: Vec<GpuChartText>,
    chart_text_buffers: ChartTextBufferPool,
    immutable_text_preparation: immutable_text_preparation::Cache,
    /// Grid for text collision avoidance
    // === CACHED GPU BUFFERS (avoid recreating every frame) ===
    /// Cached area vertex buffer (rebuilt only when geometry changes)
    cached_area_vb: Option<wgpu::Buffer>,
    cached_area_ib: Option<wgpu::Buffer>,
    area_index_upload_reuse: crate::area_index_upload_reuse::Cache<wgpu::Device>,
    cached_area_index_count: u32,
    /// Cached line vertex buffer
    exact_line_quad_enabled: bool,
    primary_line_uploads: u64,
    primary_line_repacks: u64,
    exact_line_quad_pipelines: Option<crate::exact_line_quad::Pipelines>,
    exact_line_quad_work: crate::exact_line_quad::Work,
    cached_line_compact: bool,
    exact_line_cpu_owner: Option<std::sync::Arc<()>>,
    cached_line_vb: Option<wgpu::Buffer>,
    cached_line_ib: Option<wgpu::Buffer>,
    immutable_line_topology: crate::immutable_line_topology::Cache,
    cached_line_index_count: u32,
    /// Cached pattern vertex buffer
    cached_pattern_vb: Option<wgpu::Buffer>,
    cached_pattern_ib: Option<wgpu::Buffer>,
    cached_pattern_index_count: u32,
    /// Whether cached GPU buffers are stale and need rebuild
    /// Immutable INDEX-only quad topology, shared by all instanced symbol batches.
    /// Never rewritten, including during candidate publication.
    shared_symbol_quad_index_buffer: Option<wgpu::Buffer>,
    immutable_instance_cache:
        Option<crate::immutable_payload_cache::ImmutablePayloadCache<wgpu::Buffer>>,
    /// Cached symbol GPU buffers per priority range (avoid recreating every frame)
    // === INSTRUCTION CACHE (reused across rebuilds when only view changes) ===
    /// Content- and visibility-aware shared suppression plan.
    // === PROFILING ===
    /// CPU-side performance profiler
    pub cpu_profiler: CpuProfiler,
    /// GPU-side profiler (wgpu-profiler)
    gpu_profiler: GpuProfilerWrapper,
    gpu_timestamp_batch: Option<crate::gpu_frame_timestamp::Batch>,
    /// Independent display scaler identity; native-route packet absence cannot clear it.
    // === BACKGROUND WORLD MAP (Natural Earth) ===
    /// Pre-parsed coastline segments from Natural Earth 110m GeoJSON
    /// Each inner Vec is a line string: list of [longitude, latitude] pairs
    world_map_coastlines: Arc<ferrite_render::BackgroundCoastlines>,
    world_map_detailed: Arc<ferrite_render::BackgroundCoastlines>,

    /// World map line vertices (separate from chart line_vertices)
    /// Opaque background rectangles over chart bboxes (mask world map under charts)
    /// Cached GPU buffers for world map
    cached_wm_line_vb: Option<wgpu::Buffer>,
    cached_wm_line_ib: Option<wgpu::Buffer>,
    cached_wm_mask_vb: Option<wgpu::Buffer>,
    cached_wm_mask_ib: Option<wgpu::Buffer>,
    // === LONGITUDE WRAPPING (infinite horizontal panning) ===
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
        let immutable_line_topology = crate::immutable_line_topology::Cache::prepare(
            std::env::var_os("FERRITE_IMMUTABLE_LINE_TOPOLOGY").as_deref(),
            &state,
        )
        .await;
        let gpu_profiler = GpuProfilerWrapper::new(&state.device);
        // Exact immutable-source projection cache: bounded to 32 MiB; stale/unsupported
        // inputs decline to the original projection. Explicit 0 still disables it.
        let source_line_arena_policy = std::env::var_os("FERRITE_SOURCE_LINE_PROJECTION_ARENA")
            .unwrap_or_else(|| std::ffi::OsString::from("1"));
        let source_line_projection_arena =
            crate::source_line_projection_arena::Cache::new_with_bounds(
                Some(source_line_arena_policy.as_os_str()),
                std::env::var_os("FERRITE_SOURCE_LINE_ARENA_BOUNDS").as_deref(),
            );
        let moving_flag = std::env::var_os("FERRITE_MOVING_LINE_NORTHING_REUSE");
        let moving_line_northing = crate::moving_line_northing::Cache::new(
            source_line_projection_arena.moving_flag(moving_flag.as_deref()),
        );
        let retained_world_areas = crate::retained_world_area::RetainedWorldAreas::new(
            std::env::var_os("FERRITE_RETAINED_WORLD_AREAS").as_deref(),
        );
        let area_projection_shadow =
            crate::retained_world_area::ExactAreaShadowCache::new(retained_world_areas.enabled());

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

        let suppression_tail_enabled =
            std::env::var("FERRITE_SUPPRESSION_TAIL_DIAGNOSTICS").as_deref() == Ok("1");
        let exact_line_quad_enabled = crate::exact_line_quad::enabled(
            std::env::var_os("FERRITE_EXACT_LINE_QUAD_INSTANCING").as_deref(),
        );
        let primary_line_quad_enabled = exact_line_quad_enabled
            && crate::exact_line_quad::enabled(
                std::env::var_os("FERRITE_PRIMARY_LINE_QUADS").as_deref(),
            );
        Ok(WgpuRenderer {
            vector_emission: VectorEmissionOwned {
                scene_draw_plan: vector_scene_draw::scene_draw_plan::Cache::new(
                    std::env::var_os("FERRITE_SCENE_DRAW_PLAN").as_deref(),
                ),
                primary_line_quad_enabled,
                accepted_screen_line_packet: crate::accepted_screen_line_packet::Capture::new(
                    std::env::var_os("FERRITE_ACCEPTED_SCREEN_LINE_PACKET").as_deref(),
                ),
                area_projection_shadow,
                area_triangulation_reuse_enabled: ferrite_render::area_triangulation_reuse_enabled(
                ),
                cached_symbol_buffers: Vec::new(),
                compact_owner_admission_diagnostics: crate::compact_owner_admission::enabled(
                    std::env::var_os("FERRITE_COMPACT_OWNER_ADMISSION_DIAGNOSTICS").as_deref(),
                ),
                compact_owner_admission_enabled: crate::compact_owner_admission::enabled(
                    std::env::var_os("FERRITE_COMPACT_OWNER_ADMISSION").as_deref(),
                ),
                current_frame_admission_reuse_enabled: std::env::var_os(
                    "FERRITE_CURRENT_FRAME_ADMISSION_REUSE",
                )
                .as_deref()
                    == Some(std::ffi::OsStr::new("1")),
                compact_owner_work: Default::default(),
                coverage_pipelines: None,
                coverage_trial_reuse: crate::coverage_trial::enabled(
                    std::env::var_os("FERRITE_COVERAGE_TRIAL_REUSE").as_deref(),
                ),
                coverage_trial_work: Default::default(),
                emitter_wave_work: crate::emitter_wave_diagnostics::collector_with_ordered_lines(
                    std::env::var_os("FERRITE_EMITTER_WAVE_DIAGNOSTICS").as_deref(),
                    std::env::var_os("FERRITE_ORDERED_LINE_WAVE_CENSUS").as_deref(),
                ),
                empty_symbol_ref_count: 0,
                flat_diagnostic: None,
                flat_gpu_coverage_host_ns: 0,
                flat_stage_timing_enabled:
                    ferrite_render::flat_reuse_diagnostics::stage_timing_enabled(
                        std::env::var_os("FERRITE_FLAT_STAGE_TIMING").as_deref(),
                    ),
                frame_local_coverage_clip_reuse:
                    crate::coverage_gpu_frame::frame_local_clip_reuse_policy(
                        std::env::var_os("FERRITE_FLAT_COVERAGE_CLIP_CSE").as_deref(),
                    ),
                gpu_buffers_dirty: true,
                gpu_timestamp_requested: crate::gpu_frame_timestamp::requested(
                    std::env::var_os("FERRITE_GPU_FRAME_TIMESTAMPS").as_deref(),
                ),
                gpu_timestamp_target_camera: None,
                line_preparation_work: crate::line_preparation_diagnostics::enabled(
                    std::env::var_os("FERRITE_LINE_PREPARATION_DIAGNOSTICS").as_deref(),
                )
                .then(|| crate::shared_cell::Shared::new(Default::default())),
                line_suppression: {
                    let mut cache = ferrite_render::LineSuppressionCache::default();
                    cache.set_tail_diagnostics_enabled(suppression_tail_enabled);
                    cache
                },
                missing_symbol_ids: FxHashSet::with_capacity_and_hasher(32, Default::default()),
                moving_line_northing,
                source_batch_parallel: crate::source_batch_parallel::Cache::new(
                    std::env::var_os("FERRITE_SOURCE_BATCH_PARALLEL").as_deref(),
                ),
                native_route_target_camera: None,
                owner_draw_borrow_enabled: std::env::var("FERRITE_OWNER_DRAW_BORROW")
                    .is_ok_and(|value| value == "1"),
                owner_group_plan_diagnostics: std::env::var("FERRITE_OWNER_GROUP_DIAGNOSTICS")
                    .is_ok_and(|v| v == "1"),
                owner_group_plan_enabled: std::env::var("FERRITE_OWNER_GROUP_PLAN")
                    .is_ok_and(|v| v == "1"),
                owner_group_work: Default::default(),
                pattern_textures: HashMap::new(),
                referenced_chart_owner: None,
                retained_area_triangulations: AreaTriangulationRetention::default(),
                retained_world_areas,
                source_line_projection_arena,
                spatial_hierarchy_enabled: false,
                dense_area_candidates_baseline: crate::area_candidates::dense_baseline_flag(
                    std::env::var_os("FERRITE_DENSE_AREA_CANDIDATES_BASELINE").as_deref(),
                ),
                static_line_bounds: crate::static_line_bounds::Cache::new(
                    std::env::var_os("FERRITE_STATIC_LINE_BOUNDS_CSE").as_deref(),
                ),
                static_source_classification_enabled: std::env::var(
                    "FERRITE_STATIC_SOURCE_CLASSIFICATION",
                )
                .is_ok_and(|v| v == "1"),
                suppression_tail: suppression_tail_enabled
                    .then(crate::suppression_tail::Collector::default),
                symbol_class_cache: FxHashMap::with_capacity_and_hasher(256, Default::default()),
                symbol_textures: HashMap::with_capacity(100),
                triangulation_cache: HashMap::with_capacity(500),
                triangulation_failures: FxHashSet::default(),
                triangulation_revision: None,
                vector_frame: VectorFrameState {
                    frame_cpu: VectorGeometryCpu {
                        world_map_line_vertices: Vec::with_capacity(2000),
                        world_map_line_indices: Vec::with_capacity(6000),
                        world_map_mask_vertices: Vec::new(),
                        world_map_mask_indices: Vec::new(),
                        area_vertices: Vec::with_capacity(10000),
                        area_indices: Vec::with_capacity(30000),
                        line_geometry: crate::primary_line_geometry::Geometry::new_frame(
                            5000,
                            15000,
                            primary_line_quad_enabled,
                        ),
                        symbol_instances: Vec::with_capacity(2000),
                        pattern_vertices: Vec::with_capacity(5000),
                        pattern_indices: Vec::with_capacity(15000),
                        text_labels: Vec::with_capacity(500),
                        area_priority_ranges: Vec::with_capacity(10),
                        line_priority_ranges: Vec::with_capacity(10),
                        symbol_priority_ranges: Vec::with_capacity(10),
                        pattern_ranges: Vec::with_capacity(10),
                        displayed_geometry: Vec::new(),
                        pattern_emission_audit: (crate::background_test::enabled()
                            && std::env::var("FERRITE_PATTERN_EMISSION_AUDIT").as_deref()
                                == Ok("1"))
                        .then(Vec::new),
                        pattern_emission_audit_dropped: 0,
                        owner: Arc::new(()),
                    },
                    geometry_transform: None,
                    temporal_visibility_counts: (0, 0),
                    temporal_visibility_mask: Vec::new(),
                    display_scale: 1,
                    coverage_scale_colour: None,
                    coverage_scale_colours: Default::default(),
                    screen_pan_offset: (0.0, 0.0),
                    selection_anchor: None,
                    selection_world_geometry: Vec::new(),
                    selection_screen_geometry: Vec::new(),
                    selection_index: std::cell::OnceCell::new(),
                    dependency_status: DependencyRenderStatus::default(),
                    screen_zoom_scale: 1.0,
                    screen_zoom_scale_y: 1.0,
                    screen_zoom_pivot: (0.0, 0.0),
                    view_dependent_symbols: false,
                    view_clipped_patterns: false,
                    scene_bounds: None,
                    viewport_world_bounds: None,
                    prepared_coverage: None,
                    overscale_annotation: Vec::new(),
                    coverage_frame: None,
                    coverage_failed: false,
                    emitting_coverage_source: None,
                    device_fixed_sources: FxHashSet::default(),
                    static_source_classification: None,
                    chart_geometry_viewport: None,
                    world_map_chart_boxes: Vec::new(),
                    lon_wrap_screen_px: 0.0,
                },
            },

            draw_range_index_enabled: std::env::var("FERRITE_DRAW_RANGE_INDEX").as_deref()
                == Ok("1"),
            state,
            pipelines,
            view_buffer,
            view_bind_group,
            // Pre-allocate with typical initial capacities to avoid reallocation
            raster_layers: Vec::new(),
            continuous_owner: std::sync::Arc::new(()),
            raster_epoch: Arc::new(()),
            vector_scene_epoch: Default::default(),
            continuous_uniforms: [Some(uniforms), None, None],
            continuous_frame: None,
            continuous_layer_count: 0,
            raster_enabled_groups: None,

            background_color: Color::from_u8(201, 237, 255, 255), // DEPDW (deep water) — matches S-101 default
            symbol_scale: 1.0, // S-100 standard: 1.0 = nominal symbol size at 0.3mm/pixel
            show_soundings: true, // Visibility controlled by S-101 viewing groups
            zoom_level: 1.0,
            compilation_scale: 22000, // Default compilation scale (1:22000)

            empty_selection_stats: ferrite_render::SelectionIndexStats::default(),
            egui,
            ui_state: AppUiState::default(),
            // Optimization fields
            packed_symbol_vertices: Vec::with_capacity(4000),
            packed_symbol_indices: Vec::with_capacity(6000),
            packed_symbol_ranges: Vec::with_capacity(50),
            animation_mode: false,
            motion_preview: crate::motion_preview::enabled(
                std::env::var_os("FERRITE_MOTION_PREVIEW").as_deref(),
            ),
            motion_preview_active: false,
            idle_builder: None,
            scene_generation: 0,
            scene_build_pending: false,
            preview_refresh_due: false,
            worker_fonts: None,

            overscale_program_reuse: Arc::new(crate::overscale_annotation::ProgramReuse::new(
                std::env::var_os("FERRITE_OVERSCALE_PROGRAM_REUSE").as_deref(),
            )),

            native_route_gpu_owner: crate::NativeRouteGpuOwner::new(),
            native_route_gpu: None,

            native_route_last_encoded: std::cell::Cell::new(false),

            chart_text_shapes: Vec::new(),
            chart_text_meshes: Vec::new(),

            chart_text_buffers: ChartTextBufferPool::default(),
            immutable_text_preparation: immutable_text_preparation::Cache::new(
                std::env::var_os("FERRITE_IMMUTABLE_TEXT_PREPARATION").as_deref(),
            ),
            // GPU buffer cache
            cached_area_vb: None,
            cached_area_ib: None,
            area_index_upload_reuse: crate::area_index_upload_reuse::Cache::new(
                std::env::var_os("FERRITE_AREA_INDEX_UPLOAD_REUSE").as_deref(),
            ),
            cached_area_index_count: 0,
            exact_line_quad_enabled,
            primary_line_uploads: 0,
            primary_line_repacks: 0,
            exact_line_quad_pipelines: None,
            exact_line_quad_work: Default::default(),
            cached_line_compact: false,
            exact_line_cpu_owner: None,
            cached_line_vb: None,
            cached_line_ib: None,
            immutable_line_topology,
            cached_line_index_count: 0,
            cached_pattern_vb: None,
            cached_pattern_ib: None,
            cached_pattern_index_count: 0,

            shared_symbol_quad_index_buffer: None,
            immutable_instance_cache: (std::env::var("FERRITE_IMMUTABLE_INSTANCE_CACHE")
                .as_deref()
                == Ok("1"))
            .then(crate::immutable_payload_cache::ImmutablePayloadCache::new),

            // Profiling
            cpu_profiler: CpuProfiler::new(),
            gpu_profiler,
            gpu_timestamp_batch: None,

            // Background world map (empty until set_world_map is called)
            world_map_coastlines: Default::default(),
            world_map_detailed: Default::default(),

            cached_wm_line_vb: None,
            cached_wm_line_ib: None,
            cached_wm_mask_vb: None,
            cached_wm_mask_ib: None,
            // Longitude wrapping
            view_buffer_left,
            view_buffer_right,
            view_bind_group_left,
            view_bind_group_right,
        })
    }

    /// Enable or disable profiling (both CPU and GPU)
    pub fn set_profiling_enabled(&mut self, enabled: bool) {
        crate::profiler::set_profiling_enabled(enabled);
        self.cpu_profiler.reset_debug_metrics();
        self.ui_state.debug_cpu_frames = Default::default();
        self.ui_state.debug_gpu.chart_ms = None;
        self.ui_state.debug_gpu.chart_max_ms = None;
        self.ui_state.debug_gpu.chart_window_samples = 0;
        self.ui_state.debug_gpu.sample_age_seconds = None;
        self.gpu_profiler.set_enabled(enabled);
    }

    /// Flush profiler reports (call on shutdown)
    pub fn flush_profiler(&mut self) {
        self.cpu_profiler.flush();
    }

    /// Set animation mode for fast-path rendering during drag/zoom
    #[inline]
    pub fn set_animation_mode(&mut self, animating: bool) {
        self.vector_scene_epoch.advance();
        self.animation_mode = animating;
    }

    /// CPU chart geometry must have been built with the currently requested view.
    pub fn geometry_matches_view(&self, scaler: &ferrite_render::Scaler) -> bool {
        self.vector_emission.vector_frame.geometry_transform == Some(Self::scaler_transform(scaler))
    }

    /// Readiness only: does not authorize GPU reuse or weaken visibility guards.
    pub fn navigation_scene_ready(&self, scaler: &ferrite_render::Scaler) -> bool {
        !self.vector_emission.vector_frame.coverage_failed
            && self.geometry_matches_view(scaler)
            && self.validate_continuous_frame().is_ok()
    }

    /// Update viewport world bounds for frustum culling
    pub fn update_viewport_bounds(&mut self, scaler: &ferrite_render::Scaler) {
        self.vector_scene_epoch.advance();
        let mut services = EmissionServices::live(
            &self.state,
            &self.pipelines,
            &mut self.egui,
            &mut self.cpu_profiler,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission
            .update_viewport_bounds(&mut services, scaler)
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
        self.vector_emission.triangulation_failures.len()
    }
    pub fn clear_triangulation_cache(&mut self) {
        self.invalidate_scene_builder();
        self.vector_emission.area_projection_shadow.reset();
        self.vector_emission.moving_line_northing.reset();
        self.vector_emission.static_line_bounds.reset();
        self.vector_emission.triangulation_cache.clear();
        self.vector_emission.triangulation_failures.clear();
        self.vector_emission.triangulation_revision = None;
        self.vector_emission.retained_area_triangulations.reset();
        self.vector_emission.line_suppression.clear();
    }

    fn bind_triangulation_context(&mut self, context: &RenderContext) {
        let mut services = EmissionServices::live(
            &self.state,
            &self.pipelines,
            &mut self.egui,
            &mut self.cpu_profiler,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission
            .bind_triangulation_context(&mut services, context)
    }

    /// Pre-compute triangulations for all area instructions.
    /// Call after chart load to avoid cold-path stalls during first render frame.
    pub fn precompute_triangulations(&mut self, context: &RenderContext) {
        self.bind_triangulation_context(context);
        let mut count = 0;
        for instr in context.raw_instructions() {
            if let ferrite_render::DrawingInstruction::Area(area) = instr {
                if self.vector_emission.area_triangulation_reuse_enabled {
                    self.vector_emission
                        .retained_area_triangulations
                        .precomputed_areas = self
                        .vector_emission
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
        if self.vector_emission.area_triangulation_reuse_enabled {
            // Drop old retention before recapture; no two retained generations.
            self.vector_emission.retained_area_triangulations.reset();
            let projection = context.scaler.projection();
            for (ordinal, instruction) in context.raw_instructions().iter().enumerate() {
                let ferrite_render::DrawingInstruction::Area(area) = instruction else {
                    continue;
                };
                let key = Self::area_geometry_key(area, projection);
                let result = if let Some(TriangulationStorage::Shared(v)) =
                    self.vector_emission.triangulation_cache.get(&key)
                {
                    Some(AreaRetainedResult::Ready(Arc::clone(v)))
                } else if self.vector_emission.triangulation_failures.contains(&key) {
                    Some(AreaRetainedResult::Rejected)
                } else {
                    None
                };
                if let Some(result) = result {
                    self.vector_emission.retained_area_triangulations.admit(
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
            self.vector_emission.retained_area_triangulations.epoch =
                Some(context.static_area_geometry_epoch());
        }
        tracing::info!("Pre-computed {} area triangulations", count);
    }

    /// Logical retained capacities, not total renderer RSS or original cache size.
    pub fn area_triangulation_reuse_statistics(
        &self,
    ) -> (bool, usize, usize, u64, u64, u64, u64, u64, usize) {
        let r = &self.vector_emission.retained_area_triangulations;
        (
            self.vector_emission.area_triangulation_reuse_enabled,
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
        self.vector_emission.vector_frame.lon_wrap_screen_px > 0.0
    }

    pub fn selected_geometry_vertex_count(&self) -> usize {
        self.vector_emission
            .vector_frame
            .selection_world_geometry
            .iter()
            .map(Vec::len)
            .sum()
    }

    pub fn displayed_geometry(&self) -> &[usize] {
        &self
            .vector_emission
            .vector_frame
            .frame_cpu
            .displayed_geometry
    }

    /// Conservative broad-phase candidates in original displayed order.
    /// Stale transforms fall back to the complete current draw list.
    pub fn selection_candidates(
        &self,
        scaler: &ferrite_render::Scaler,
        query: ferrite_render::ScreenPoint,
        radius: f64,
    ) -> Vec<usize> {
        match self.vector_emission.vector_frame.selection_index.get() {
            Some(index) if index.matches_scaler(scaler) => {
                index.candidates(scaler, query, radius, self.longitude_wrapping_enabled())
            }
            _ => self
                .vector_emission
                .vector_frame
                .frame_cpu
                .displayed_geometry
                .clone(),
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
        if self.vector_emission.triangulation_revision == Some(context.geometry_revision()) {
            self.vector_emission
                .vector_frame
                .selection_index
                .get_or_init(|| {
                    let mut index = ferrite_render::SelectionIndex::default();
                    let plan = self.vector_emission.line_suppression.current();
                    index.rebuild_in_context(
                        context,
                        &self
                            .vector_emission
                            .vector_frame
                            .frame_cpu
                            .displayed_geometry,
                        |i| plan.and_then(|p| p.spans(i)),
                    );
                    index
                });
        }
        self.selection_candidates(&context.scaler, query, radius)
    }
    pub fn selection_index_stats(&self) -> &ferrite_render::SelectionIndexStats {
        self.vector_emission
            .vector_frame
            .selection_index
            .get()
            .map(|index| index.statistics())
            .unwrap_or(&self.empty_selection_stats)
    }

    pub fn set_selection_geometry(
        &mut self,
        geometry: Vec<Vec<WorldPoint>>,
        scaler: &ferrite_render::Scaler,
    ) {
        self.vector_emission.vector_frame.selection_world_geometry = geometry;
        self.update_selection(scaler);
    }

    pub fn update_selection(&mut self, scaler: &ferrite_render::Scaler) {
        if self.ui_state.selected_feature.is_none() {
            self.vector_emission
                .vector_frame
                .selection_world_geometry
                .clear();
        }

        let shift = self
            .ui_state
            .selected_feature
            .as_ref()
            .map_or(0., |f| f.longitude_shift);
        self.vector_emission.vector_frame.selection_screen_geometry = self
            .vector_emission
            .vector_frame
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
        self.vector_emission.vector_frame.selection_anchor =
            self.ui_state.selected_feature.as_ref().map(|f| {
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
    /// PC-provenance-preserving regular S102 upload. The callback can only
    /// submit tiles minted by the S102 producer; old unbound APIs stay explicit.
    #[cfg(feature = "s102-portrayal")]
    pub fn stage_bound_s102_raster_batch<E: From<crate::WgpuError>>(
        &mut self,
        scaler: &ferrite_render::Scaler,
        producer: impl FnOnce(
            &mut dyn FnMut(ferrite_s102::BoundBathymetryMaterial) -> std::result::Result<(), E>,
        ) -> std::result::Result<(), E>,
    ) -> std::result::Result<PreparedRasterMaterialBatch, E> {
        self.stage_s102_raster_batch(scaler, |upload| {
            producer(&mut |material| upload(S102RasterMaterial::Bound(material)))
        })
    }
    #[cfg(feature = "s102-portrayal")]
    pub fn stage_s102_raster_batch<E: From<crate::WgpuError>>(
        &mut self,
        scaler: &ferrite_render::Scaler,
        producer: impl FnOnce(
            &mut dyn FnMut(S102RasterMaterial) -> std::result::Result<(), E>,
        ) -> std::result::Result<(), E>,
    ) -> std::result::Result<PreparedRasterMaterialBatch, E> {
        let mut layers = Vec::new();
        let mut bytes = 0usize;
        let mut budget = crate::raster_publication_budget::RasterPublicationBudget::default();
        producer(&mut |material| {
            let (layer, owner) = match material {
                S102RasterMaterial::Bound(v) => {
                    let (layer, owner) = v.into_gpu_parts();
                    (layer, Some(owner))
                }
                S102RasterMaterial::LegacyIc(layer) => (layer, None),
            };
            bytes = bytes.checked_add(layer.rgba.len()).ok_or_else(|| {
                crate::WgpuError::Render("Bound raster batch size overflow".into())
            })?;
            if bytes > 256 * 1024 * 1024 || layers.len() >= 4096 {
                return Err(crate::WgpuError::Render(
                    "Bound raster batch exceeds 256MiB/4096-layer budget".into(),
                )
                .into());
            }
            let mut prepared = self.prepare_raster_material(layer, scaler, None)?;
            prepared.regular_portrayal_owner = owner;
            prepared.admit(&mut budget)?;
            layers.push(prepared);
            Ok(())
        })?;
        Ok(PreparedRasterMaterialBatch {
            layers,
            owner: Arc::clone(&self.continuous_owner),
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
            layer.prepared_camera = scaler.flat_encoded_identity();
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
        self.vector_emission
            .vector_frame
            .continuous_transform_key(self.state.viewport_size())
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
            layer.prepared_camera = scaler.flat_encoded_identity();
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
            #[cfg(feature = "s102-portrayal")]
            regular_portrayal_owner: None,
            prepared_camera: scaler.flat_encoded_identity(),
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
            self.raster_layers[i].prepared_camera = scaler.flat_encoded_identity();
        }
    }
    /// Update visibility without re-uploading coverage pixels or geometry.
    pub fn set_raster_enabled_groups(&mut self, groups: Option<&std::collections::HashSet<u32>>) {
        if self.raster_enabled_groups.as_ref() == groups {
            return;
        }
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
        self.invalidate_scene_builder();
        self.vector_scene_epoch.advance();
        self.vector_emission.symbol_textures.clear();
        // Patterns are rasterized with the same PC colour profile as symbols.
        self.vector_emission.pattern_textures.clear();
    }

    /// Exactly the quad/culling decision used by both packing and Parent
    /// execution evidence. Longitude copies must be considered before culling.
    fn symbol_quad(&self, instance: &SymbolInstance) -> Option<[TextureVertex; 4]> {
        let mut services = EmissionServices::read_only(
            &self.state,
            &self.pipelines,
            &self.egui.ctx,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission.symbol_quad(&mut services, instance)
    }

    /// Pack symbol instances into contiguous vertex/index arrays for single-buffer rendering.
    /// Produces packed_symbol_vertices, packed_symbol_indices, and packed_symbol_ranges.
    fn pack_symbol_batch_range(&mut self, start: usize, end: usize) {
        let mut services = EmissionServices::read_only(
            &self.state,
            &self.pipelines,
            &self.egui.ctx,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission.pack_symbols(
            &mut services,
            start,
            end,
            &mut self.packed_symbol_vertices,
            &mut self.packed_symbol_indices,
            &mut self.packed_symbol_ranges,
        );
    }

    /// Actual compute-buffer readback, only outside timed hidden qualification.
    /// CPU `area-vertices.bin` alone is not evidence of the GPU compute output.
    fn audit_retained_world_area_output(
        &self,
        directory: &std::path::Path,
        digest_only: bool,
    ) -> Result<()> {
        if !self.vector_emission.retained_world_areas.active() {
            return Ok(());
        }
        if !crate::background_test::enabled()
            || self.window().is_visible() != Some(false)
            || self.window().has_focus()
        {
            return Err(WgpuError::Render(
                "Retained world-area readback requires hidden unfocused mode".into(),
            ));
        }
        let source = self
            .cached_area_vb
            .as_ref()
            .ok_or_else(|| WgpuError::Render("Missing retained world-area output".into()))?;
        let size = self
            .vector_emission
            .vector_frame
            .frame_cpu
            .area_vertices
            .len()
            .checked_mul(std::mem::size_of::<Vertex2D>())
            .ok_or_else(|| WgpuError::Render("Retained world-area readback overflow".into()))?;
        let staging = self.state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("retained-world-area-readback"),
            size: size as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            self.state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("retained-world-area-readback-copy"),
                });
        encoder.copy_buffer_to_buffer(source, 0, &staging, 0, size as u64);
        self.state.queue.submit(Some(encoder.finish()));
        let slice = staging.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.state.device.poll(wgpu::Maintain::Wait);
        receiver
            .recv()
            .map_err(|e| WgpuError::Render(e.to_string()))?
            .map_err(|e| WgpuError::Render(e.to_string()))?;
        let bytes = slice.get_mapped_range();
        let vertices: &[Vertex2D] =
            bytemuck::try_cast_slice(&bytes).map_err(|e| WgpuError::Render(e.to_string()))?;
        // Preserve actual resident GPU bytes and complete independent CPU reference
        // BEFORE validation may return an error. Diagnostic only, bounded by admission.
        use crate::audit_buffer_export::write_buffer;
        write_buffer(directory, "area-vertices-compute", &bytes, digest_only)
            .map_err(|e| WgpuError::Render(e.to_string()))?;
        write_buffer(
            directory,
            "area-vertices-compute-legacy",
            bytemuck::cast_slice(&self.vector_emission.vector_frame.frame_cpu.area_vertices),
            digest_only,
        )
        .map_err(|e| WgpuError::Render(e.to_string()))?;
        write_buffer(
            directory,
            "area-indices-compute-reference",
            bytemuck::cast_slice(&self.vector_emission.vector_frame.frame_cpu.area_indices),
            digest_only,
        )
        .map_err(|e| WgpuError::Render(e.to_string()))?;
        let (world, uniform) = self
            .vector_emission
            .retained_world_areas
            .audit_projection_inputs();
        write_buffer(directory, "area-world-compute-input", &world, digest_only)
            .map_err(|e| WgpuError::Render(e.to_string()))?;
        if let Some(uniform) = uniform {
            write_buffer(
                directory,
                "area-camera-compute-uniform",
                &uniform,
                digest_only,
            )
            .map_err(|e| WgpuError::Render(e.to_string()))?;
        }
        let mut maximum_error = 0.0_f64;
        let mut errors = Vec::<[f64; 2]>::with_capacity(vertices.len());
        let mut bad_positions = 0usize;
        let mut bad_colors = 0usize;
        let mut first_failures = Vec::new();
        for (index, (actual, legacy)) in vertices
            .iter()
            .zip(&self.vector_emission.vector_frame.frame_cpu.area_vertices)
            .enumerate()
        {
            let error = std::array::from_fn(|axis| {
                (actual.position[axis] as f64 - legacy.position[axis] as f64).abs()
            });
            let color_bad = actual.color.map(f32::to_bits) != legacy.color.map(f32::to_bits);
            let position_bad = (0..2).any(|axis| {
                !actual.position[axis].is_finite()
                    || !error[axis].is_finite()
                    || error[axis] > 0.125
            });
            if color_bad {
                bad_colors += 1;
            }
            if position_bad {
                bad_positions += 1;
            }
            for e in error {
                if e.is_finite() {
                    maximum_error = maximum_error.max(e);
                }
            }
            errors.push(error);
            if (color_bad || position_bad) && first_failures.len() < 128 {
                first_failures.push(serde_json::json!({"vertex":index,
                    "actual_position_bits":actual.position.map(f32::to_bits),
                    "legacy_position_bits":legacy.position.map(f32::to_bits),
                    "actual_color_bits":actual.color.map(f32::to_bits),
                    "legacy_color_bits":legacy.color.map(f32::to_bits),
                    "error_f64_bits":error.map(f64::to_bits),
                    "color_bad":color_bad,"position_bad":position_bad}));
            }
        }
        let count_bad = vertices.len()
            != self
                .vector_emission
                .vector_frame
                .frame_cpu
                .area_vertices
                .len();
        // Full per-vertex error binary: two native-endian IEEE754 f64 values.
        write_buffer(
            directory,
            "area-vertices-compute-errors-f64",
            bytemuck::cast_slice(&errors),
            digest_only,
        )
        .map_err(|e| WgpuError::Render(e.to_string()))?;
        std::fs::write(directory.join("area-vertices-compute.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "actual_gpu_readback":true,"vertex_count":vertices.len(),"legacy_count":self.vector_emission.vector_frame.frame_cpu.area_vertices.len(),
            "maximum_position_error_px":maximum_error,"position_failure_count":bad_positions,
            "color_failure_count":bad_colors,"count_bad":count_bad,"first_failures":first_failures,
            "geometry_transform":self.vector_emission.vector_frame.geometry_transform,
            "area_ranges":self.vector_emission.vector_frame.frame_cpu.area_priority_ranges.iter().map(|&(p,q,a,b,s)| (p,q,a,b,s)).collect::<Vec<_>>(),
            "statistics":self.vector_emission.retained_world_areas.statistics().audit_value(),
            "cpu_reference":if digest_only {"area-vertices-compute-legacy.sha256.json"} else {"area-vertices-compute-legacy.bin"},"raw_buffers_retained":!digest_only,"timing_excluded":true,
            "error_format":"native endian f64 x2, complete ordered vertex stream",
            "uniform_format":"actual production Camera Pod 48bytes",
            "world_format":"actual production WorldVertex Pod 32bytes",
            "bound_scope":"experimental engineering admission; not normative S100 tolerance or foreground FPS"
        })).map_err(|e| WgpuError::Render(e.to_string()))?)
            .map_err(|e| WgpuError::Render(e.to_string()))?;
        drop(bytes);
        staging.unmap();
        if count_bad || bad_positions != 0 || bad_colors != 0 {
            return Err(WgpuError::Render("Retained world-area actual GPU projection/material failed; complete diagnostic buffer evidence preserved".into()));
        }
        Ok(())
    }

    /// Actual draw ranges; consecutive identical textures share a range.
    /// Explicit diagnostic export; never called by the interactive frame loop.
    pub fn audit_geometry_buffers(&self, directory: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(directory)?;
        let digest_only = crate::audit_buffer_export::digest_only_enabled();
        macro_rules! buffer {
            ($name:literal, $values:expr) => {
                crate::audit_buffer_export::write_buffer(
                    directory,
                    $name,
                    bytemuck::cast_slice($values),
                    digest_only,
                )?;
            };
        }
        self.audit_retained_world_area_output(directory, digest_only)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        buffer!(
            "area-vertices",
            &self.vector_emission.vector_frame.frame_cpu.area_vertices
        );
        buffer!(
            "area-indices",
            &self.vector_emission.vector_frame.frame_cpu.area_indices
        );
        let (line_vertices, line_indices) = self
            .vector_emission
            .vector_frame
            .frame_cpu
            .line_geometry
            .audit_buffers()
            .map_err(std::io::Error::other)?;
        buffer!("line-vertices", &line_vertices);
        buffer!("line-indices", &line_indices);
        buffer!("symbol-vertices", &self.packed_symbol_vertices);
        buffer!("symbol-indices", &self.packed_symbol_indices);
        buffer!(
            "pattern-vertices",
            &self.vector_emission.vector_frame.frame_cpu.pattern_vertices
        );
        buffer!(
            "pattern-indices",
            &self.vector_emission.vector_frame.frame_cpu.pattern_indices
        );
        let metadata = serde_json::json!({
            "area_index_upload_reuse": {
                "enabled": self.area_index_upload_reuse.enabled(),
                "max_metadata_bytes": self.area_index_upload_reuse.max_metadata_bytes(),
                "max_old_new_transient_bytes": 16 * 1024 * 1024,
                "scope": "exact ordered current area index bytes; no permission or geometry reuse",
                "work": self.area_index_upload_reuse.work(),
            },
            "scene_draw_plan": self.vector_emission.scene_draw_plan.statistics(),
            "retained_world_area": {
                "enabled": self.vector_emission.retained_world_areas.enabled(),
                "statistics": self.vector_emission.retained_world_areas.statistics().audit_value(),
                "exact_cpu_shadow": self.vector_emission.area_projection_shadow.statistics().audit_value(),
                "moving_line_northing": self.vector_emission.moving_line_northing.statistics(),
                "source_line_projection_arena": self.vector_emission.source_line_projection_arena.statistics(),
                "source_batch_parallel": self.vector_emission.source_batch_parallel.statistics(),
                "static_line_bounds": self.vector_emission.static_line_bounds.statistics(),
                "area_vertices_scope": "legacy CPU shadow; GPU compute output requires independent readback",
                "model_error_limit_px": 0.125,
                "model_scope": "CPU f32 model admission, not a proven GPU error certificate or normative S100 tolerance",
            },
            "static_source_classification": {
                "enabled": self.vector_emission.static_source_classification_enabled,
                "prepared": self.vector_emission.vector_frame.static_source_classification.is_some(),
                "scope": "Actual retained source-only ordinal classification; no native hit count or visibility/permission reuse claim",
            },
            "coverage_trial_reuse": self.coverage_trial_statistics(),
            "coverage_scale_indication": self.ui_state.coverage_scale_indication.map(|indication| serde_json::json!({
                "viewing_denominator": indication.viewing_denominator,
                "overscale_factor": indication.overscale_factor,
                "sclbr_rgba": indication.sclbr.map(|colour| colour.to_array()),
                "physical_viewport": [indication.physical_viewport.x, indication.physical_viewport.y,
                    indication.physical_viewport.width, indication.physical_viewport.height],
                "reference_scope": "actual physical viewport centre; no own-ship input",
            })),
            "geometry_transform": self.vector_emission.vector_frame.geometry_transform,
            "fast_view_transform": self.fast_view_transform(),
            "fast_view_scales": self.fast_view_scales(),
            "area_ranges": self.vector_emission.vector_frame.frame_cpu.area_priority_ranges.iter().map(|&(p,q,a,b,_)| (p,q,a,b)).collect::<Vec<_>>(),
            "line_ranges": self.vector_emission.vector_frame.frame_cpu.line_priority_ranges.iter().map(|&(p,q,a,b,_)| (p,q,a,b)).collect::<Vec<_>>(),
            "pattern_emissions": self.vector_emission.vector_frame.frame_cpu.pattern_emission_audit.as_ref().map(|records| records.iter().map(|r| serde_json::json!({
                "source_ordinal":r.source_ordinal,"vertex_start":r.vertex_start,"vertex_end":r.vertex_end,
                "index_start":r.index_start,"index_end":r.index_end,"wrap_mode":r.wrap_mode,
                "wrap_dx_screen_bits":r.wrap_dx_screen_bits,
            })).collect::<Vec<_>>()),
            "pattern_emission_dropped": self.vector_emission.vector_frame.frame_cpu.pattern_emission_audit_dropped,
            "pattern_emission_scope": "full appended CPU vertex ownership including unused earcut vertices; wrap255 mesh is subsequently reused by draw-time wrap uniforms",
            "pattern_ranges": self.vector_emission.vector_frame.frame_cpu.pattern_ranges.iter().map(|(p,q,a,b,k,w,_)| (p,q,a,b,k,w)).collect::<Vec<_>>(),
            "symbol_resource_owners": self.vector_emission.vector_frame.frame_cpu.symbol_instances.iter().map(|s| (s.source,s.cell_index,ferrite_render::resolve_symbol(s.symbol_id),s.resource_owner)).collect::<Vec<_>>(),
            "packed_symbol_ranges": self.packed_symbol_ranges.iter().map(|&(id,start,count)| (ferrite_render::resolve_symbol(id.1),start,count)).collect::<Vec<_>>(),
            "symbol_gpu_buffers": self.symbol_gpu_buffer_statistics(),
            "symbol_buffer_scope": "last packed plane/priority range; complete displayed symbols are in snapshot.json",
            "displayed_geometry": self.vector_emission.vector_frame.frame_cpu.displayed_geometry,
            "flat_internal_stage_timing_enabled": self.flat_stage_timing_enabled(),
            "dense_area_candidates_baseline": self.vector_emission.dense_area_candidates_baseline,
            "owner_group_visibility": self.vector_emission.owner_group_work,
            "current_frame_admission_reuse_enabled":self.vector_emission.current_frame_admission_reuse_enabled,
            "compact_owner_admission": {"enabled":self.vector_emission.compact_owner_admission_enabled,
                "diagnostics_enabled":self.vector_emission.compact_owner_admission_diagnostics,"work":self.vector_emission.compact_owner_work},
            "suppression_tail_diagnostics": self.vector_emission.suppression_tail,
            "line_preparation_children": self.line_preparation_statistics(),
            "immutable_line_topology":self.immutable_line_topology.statistics(),
            "exact_line_quad": {"primary_enabled":self.vector_emission.primary_line_quad_enabled,"primary_active":self.vector_emission.vector_frame.frame_cpu.line_geometry.packed().is_some(),"primary_upload_attempts":self.primary_line_uploads,"legacy_repack_attempts":self.primary_line_repacks,"line_cpu_capacity_bytes":self.vector_emission.vector_frame.frame_cpu.line_geometry.charged_cpu_bytes(),"enabled":self.exact_line_quad_enabled,"active":self.cached_line_compact,
                "scope":"exact original line math; primary CPU representation may be compact and original streams reconstructed for untimed audit; modeled payload not RSS",
                "work":&self.exact_line_quad_work},
            "emitter_wave_diagnostics": self.emitter_wave_statistics(),
            "suppression_growth_children": self.vector_emission.line_suppression.growth_compiler_work(),
            "line_eligibility_program": self.vector_emission.line_suppression.eligibility_work(),
                "selection_index": self.selection_index_stats(),
            "drawing_dependencies": self.vector_emission.vector_frame.dependency_status.audit_value(),
        });
        std::fs::write(
            directory.join("metadata.json"),
            serde_json::to_vec_pretty(&metadata)?,
        )
    }

    pub fn symbol_gpu_buffer_statistics(&self) -> serde_json::Value {
        serde_json::json!({
            "instancing_enabled": self.pipelines.symbol_instance_pipeline.is_some(),
            "vertex_bytes": self.vector_emission.cached_symbol_buffers.iter().map(|b| b.4.size()).sum::<u64>(),
            "index_bytes": if self.pipelines.symbol_instance_pipeline.is_some() { self.shared_symbol_quad_index_buffer.as_ref().map_or(0,wgpu::Buffer::size) } else { self.vector_emission.cached_symbol_buffers.iter().map(|b| b.5.size()).sum::<u64>() },
            "shared_quad_index_buffers": usize::from(self.shared_symbol_quad_index_buffer.is_some()),
            "buffer_pairs": self.vector_emission.cached_symbol_buffers.len(),
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
        self.vector_emission
            .cached_symbol_buffers
            .iter()
            .map(|batch| batch.6.len())
            .sum()
    }

    /// Hidden-only actual instance/quad census after the current frame is prepared.
    /// CPU draw inputs, not a direct GPU resident-buffer readback.
    pub fn audit_point_preservation(&self, directory: &std::path::Path) -> Result<()> {
        if !crate::background_test::enabled()
            || self.window().is_visible() != Some(false)
            || self.window().has_focus()
        {
            return Err(WgpuError::Render(
                "Point audit requires hidden unfocused mode".into(),
            ));
        }
        const MAX_INSTANCES: usize = 4_096;
        if self
            .vector_emission
            .vector_frame
            .frame_cpu
            .symbol_instances
            .len()
            > MAX_INSTANCES
        {
            return Err(WgpuError::Render(
                "Point audit instance limit exceeded; no partial census".into(),
            ));
        }
        let rows = self
            .vector_emission
            .vector_frame
            .frame_cpu
            .symbol_instances
            .iter()
            .enumerate()
            .map(|(ordinal, s)| {
                let quad = self.symbol_quad(s);
                serde_json::json!({
                    "instance_ordinal": ordinal, "source_ordinal": s.source,
                    "symbol_ref": ferrite_render::resolve_symbol(s.symbol_id),
                    "feature_id": s.feature_id, "cell_index": s.cell_index,
                    "resource_owner": s.resource_owner, "plane": s.plane, "priority": s.priority,
                    "world_bits": [s.world.x.to_bits(), s.world.y.to_bits()],
                    "screen_bits": [s.screen_x.to_bits(), s.screen_y.to_bits()],
                    "anchor_bits": [s.anchor[0].to_bits(), s.anchor[1].to_bits()],
                    "scale_bits": s.scale.to_bits(), "rotation_bits": s.rotation.to_bits(),
                    "quad_eligible": quad.is_some(),
                    "quad_bytes": quad.as_ref().map(|q| bytemuck::cast_slice::<_,u8>(q).to_vec()),
                })
            })
            .collect::<Vec<_>>();
        let value = serde_json::json!({
            "schema": 1, "hidden": true, "focused": false,
            "complete": true, "limit": MAX_INSTANCES, "rows": rows,
            "scope": "actual renderer instances and shared packed/instanced quad CPU inputs; no direct GPU byte proof",
        });
        std::fs::create_dir_all(directory).map_err(|e| WgpuError::Render(e.to_string()))?;
        std::fs::write(
            directory.join("point-preservation.json"),
            serde_json::to_vec_pretty(&value).map_err(|e| WgpuError::Render(e.to_string()))?,
        )
        .map_err(|e| WgpuError::Render(e.to_string()))
    }

    /// Symbols accepted by the same visibility and collision filters as drawing.
    pub fn displayed_symbols(&self) -> Vec<DisplayedSymbol> {
        self.vector_emission
            .vector_frame
            .frame_cpu
            .symbol_instances
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
        self.vector_emission
            .vector_frame
            .frame_cpu
            .symbol_instances
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
            self.vector_emission.vector_frame.screen_pan_offset,
            self.vector_emission.vector_frame.screen_zoom_scale,
            self.vector_emission.vector_frame.screen_zoom_pivot,
        )
    }

    /// Handle window resize
    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.scene_generation = self.scene_generation.wrapping_add(1);
        self.vector_emission.native_route_target_camera = None;
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
        self.vector_scene_epoch.advance();
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
        self.vector_scene_epoch.advance();
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
        self.ui_state.publish_plugin_ui_data(data);
    }

    /// Take pending plugin UI events
    #[inline]
    pub fn take_plugin_ui_events(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.ui_state.plugin_ui_events)
    }

    /// Update view uniforms after resize or zoom
    fn update_view_uniforms(&mut self) {
        self.vector_scene_epoch.advance();
        let uniforms = self
            .vector_emission
            .vector_frame
            .view_uniforms(self.state.viewport_size());
        self.continuous_uniforms = uniforms;
        self.state
            .update_view_uniforms(&self.view_buffer, &uniforms[0].expect("central view"));
        if let Some(left) = uniforms[1] {
            self.state
                .update_view_uniforms(&self.view_buffer_left, &left);
        }
        if let Some(right) = uniforms[2] {
            self.state
                .update_view_uniforms(&self.view_buffer_right, &right);
        }
        self.update_coverage_transform();
    }

    fn coverage_clip_transform(&self) -> Result<crate::coverage_clip::ClipTransform> {
        let mut services = EmissionServices::read_only(
            &self.state,
            &self.pipelines,
            &self.egui.ctx,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission.coverage_clip_transform(&mut services)
    }

    /// Transform a retained symbol anchor exactly as its vertex shader does.
    pub fn displayed_symbol_screen(&self, anchor: [f32; 2], pass: usize) -> Option<[f32; 2]> {
        let wrap = match pass {
            0 => 0.,
            1 if self.longitude_wrapping_enabled() => {
                -self.vector_emission.vector_frame.lon_wrap_screen_px
            }
            2 if self.longitude_wrapping_enabled() => {
                self.vector_emission.vector_frame.lon_wrap_screen_px
            }
            _ => return None,
        };
        let (px, py) = self.vector_emission.vector_frame.screen_zoom_pivot;
        let point = [
            (anchor[0] + self.vector_emission.vector_frame.screen_pan_offset.0 + wrap - px)
                * self.vector_emission.vector_frame.screen_zoom_scale
                + px,
            (anchor[1] + self.vector_emission.vector_frame.screen_pan_offset.1 - py)
                * self.vector_emission.vector_frame.screen_zoom_scale_y
                + py,
        ];
        point.iter().all(|v| v.is_finite()).then_some(point)
    }

    /// Sample the retained draw mask using the same inverse affine as the GPU.
    pub fn coverage_fragment_visible(&self, index: usize, pass: usize, pixel: [f32; 2]) -> bool {
        if self.vector_emission.vector_frame.coverage_failed
            || (pass != 0 && self.is_device_fixed_source(index))
            || !pixel.iter().all(|v| v.is_finite())
        {
            return false;
        }
        let Some(prepared) = &self.vector_emission.vector_frame.prepared_coverage else {
            return true;
        };
        let Some(frame) = &self.vector_emission.vector_frame.coverage_frame else {
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
        let mut services = EmissionServices::live(
            &self.state,
            &self.pipelines,
            &mut self.egui,
            &mut self.cpu_profiler,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission
            .update_coverage_transform(&mut services)
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
            [
                self.vector_emission.vector_frame.screen_zoom_scale,
                self.vector_emission.vector_frame.screen_zoom_scale_y,
            ],
            [dx, dy],
            [
                self.vector_emission.vector_frame.screen_zoom_pivot.0,
                self.vector_emission.vector_frame.screen_zoom_pivot.1,
            ],
        ) {
            return false;
        }
        self.vector_emission.native_route_target_camera = None;
        self.vector_emission.vector_frame.screen_pan_offset = (dx, dy);
        self.update_view_uniforms();
        true
    }

    /// Add a pan, returning None when geometry must be rebuilt or the
    /// composed transform is invalid. Rejection never changes the old offset.
    pub fn add_pan_offset(&mut self, dx: f32, dy: f32) -> Option<(f32, f32)> {
        let pan = (
            self.vector_emission.vector_frame.screen_pan_offset.0 + dx,
            self.vector_emission.vector_frame.screen_pan_offset.1 + dy,
        );
        self.set_pan_offset(pan.0, pan.1).then_some(pan)
    }

    /// Get the current screen-space pan offset
    #[inline]
    pub fn get_pan_offset(&self) -> (f32, f32) {
        self.vector_emission.vector_frame.screen_pan_offset
    }

    /// Reset pan offset to zero (call before full rebuild)
    #[inline]
    pub fn reset_pan_offset(&mut self) {
        self.vector_emission.vector_frame.screen_pan_offset = (0.0, 0.0);
        self.vector_emission.vector_frame.screen_zoom_scale = 1.0;
        self.vector_emission.vector_frame.screen_zoom_scale_y = 1.0;
        self.vector_emission.vector_frame.screen_zoom_pivot = (0.0, 0.0);
        self.update_view_uniforms();
    }

    /// Set GPU zoom only for geometry that supports an affine navigation.
    /// False means a full rebuild is required; no state changes on rejection.
    #[inline]
    pub fn set_gpu_zoom(&mut self, scale: f32, pivot_x: f32, pivot_y: f32) -> bool {
        if !self.accepts_gpu_navigation(
            [scale, scale],
            [
                self.vector_emission.vector_frame.screen_pan_offset.0,
                self.vector_emission.vector_frame.screen_pan_offset.1,
            ],
            [pivot_x, pivot_y],
        ) {
            return false;
        }
        self.vector_emission.vector_frame.screen_zoom_scale = scale;
        self.vector_emission.vector_frame.screen_zoom_scale_y = scale;
        self.vector_emission.native_route_target_camera = None;
        self.vector_emission.vector_frame.screen_zoom_pivot = (pivot_x, pivot_y);
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
        self.requires_exact_rebuild_for_navigation()
            || (!self.motion_preview && self.preview_differs_from_exact_navigation())
    }

    /// Blockers that cannot be shown even approximately through the affine.
    fn requires_exact_rebuild_for_navigation(&self) -> bool {
        // The experimental error gate certifies the current identity fast-view
        // pose only. Do not magnify its error using the old global affine route.
        self.vector_emission.retained_world_areas.active()
            || self
                .vector_emission
                .vector_frame
                .dependency_status
                .iterations
                > 0
            || self.vector_emission.vector_frame.view_clipped_patterns
    }

    /// The affine shows retained placement/coverage, which differs from an
    /// exact rebuild at the target camera. Allowed only as motion preview.
    fn preview_differs_from_exact_navigation(&self) -> bool {
        // A currently required S-98 per-region annotation must be rebuilt
        // with selected coverage before any consumer reads the scene.
        !self.vector_emission.vector_frame.overscale_annotation.is_empty()
            // Current absence of a pattern does not prove absence at the next
            // camera. Re-select annotated ENC coverage after the motion settles.
            || self.vector_emission.vector_frame.prepared_coverage.as_ref().is_some_and(|coverage| {
                coverage.requires_scale_selection_rebuild()
            })
            || self.vector_emission.vector_frame.view_dependent_symbols
    }

    /// True while the visible affine is an approximate motion preview; the
    /// caller must rebuild before picking, exporting or auditing the scene.
    pub fn motion_preview_active(&self) -> bool {
        self.motion_preview_active
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
        self.state
            .surface_pacing_arm(frame, source_epoch, view_epoch);
        self.vector_emission.flat_gpu_coverage_host_ns = 0;
        self.vector_emission.flat_diagnostic = Some(crate::shared_cell::Shared::new(
            ferrite_render::flat_reuse_diagnostics::FlatFrameSample::new(
                frame,
                source_epoch,
                view_epoch,
                true,
                false,
            ),
        ));
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
            .vector_emission
            .flat_diagnostic
            .take()
            .ok_or_else(|| WgpuError::Render("No diagnostic frame".into()))?;
        let mut row = *cell.borrow();
        row.internal_stage_timing_available = self.vector_emission.flat_stage_timing_enabled;
        row.service_ns = service_ns;
        row.work.dependency_iterations = self
            .vector_emission
            .vector_frame
            .dependency_status
            .iterations as u64;
        Ok(row)
    }
    fn is_device_fixed_source(&self, ordinal: usize) -> bool {
        let mut services = EmissionServices::read_only(
            &self.state,
            &self.pipelines,
            &self.egui.ctx,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission
            .is_device_fixed_source(&mut services, ordinal)
    }

    fn flat_span(
        &self,
        stage: ferrite_render::flat_reuse_diagnostics::FlatFrameStage,
    ) -> Option<FlatDiagnosticSpan> {
        let mut services = EmissionServices::read_only(
            &self.state,
            &self.pipelines,
            &self.egui.ctx,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission.flat_span(&mut services, stage)
    }
    pub fn flat_gpu_coverage_host_ns(&self) -> u64 {
        self.vector_emission.flat_gpu_coverage_host_ns
    }
    /// (enabled, alias bindings, unique clips, legacy logical R8 charge, unique R8 payload).
    /// Current frame only; no cache identity or resources survive replacement.
    pub fn coverage_clip_cse_statistics(&self) -> (bool, usize, usize, usize, usize) {
        self.vector_emission
            .vector_frame
            .coverage_frame
            .as_ref()
            .map_or(
                (
                    self.vector_emission.frame_local_coverage_clip_reuse,
                    0,
                    0,
                    0,
                    0,
                ),
                |frame| {
                    (
                        self.vector_emission.frame_local_coverage_clip_reuse,
                        frame.mask_count(),
                        frame.unique_clip_count(),
                        frame.pixel_bytes(),
                        frame.unique_pixel_bytes(),
                    )
                },
            )
    }

    /// Per-frame bounded HOST work counters; not GPU duration or FPS.
    /// Renderer-lifetime bounded immutable program reuse counts; HOST only.
    pub fn overscale_program_statistics(&self) -> serde_json::Value {
        self.overscale_program_reuse.statistics()
    }
    pub fn coverage_trial_statistics(&self) -> serde_json::Value {
        self.vector_emission
            .coverage_trial_work
            .snapshot(self.vector_emission.coverage_trial_reuse)
    }
    /// Exact constructor-sampled policy. A sample only becomes available with an
    /// active collector; finish_flat_diagnostics records this mode into its row.
    pub fn flat_stage_timing_enabled(&self) -> bool {
        self.vector_emission.flat_stage_timing_enabled
    }
    /// Current callback availability, independently from counters/collector activity.
    pub fn flat_internal_stage_timings_available(&self) -> bool {
        self.vector_emission.flat_stage_timing_enabled
            && self.vector_emission.flat_diagnostic.is_some()
    }
    pub fn flat_diagnostics_active(&self) -> bool {
        self.vector_emission.flat_diagnostic.is_some()
    }
    pub fn record_flat_stage(
        &self,
        stage: ferrite_render::flat_reuse_diagnostics::FlatFrameStage,
        elapsed: std::time::Duration,
    ) {
        let mut services = EmissionServices::read_only(
            &self.state,
            &self.pipelines,
            &self.egui.ctx,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission
            .record_flat_stage(&mut services, stage, elapsed)
    }
    /// `previewed`: the affine was accepted as a motion preview, so the
    /// preview-only blockers did not reject this attempt.
    fn record_flat_reuse(
        &mut self,
        affine: Option<ferrite_render::flat_reuse_diagnostics::NavigationAffine>,
        previewed: bool,
    ) {
        if self.vector_emission.flat_diagnostic.is_some() || crate::profiler::is_profiling_enabled()
        {
            use ferrite_render::flat_reuse_diagnostics::*;
            let reason = classify_flat_reuse(FlatNavigationInputs {
                mode_supported: true,
                retained_world_area: self.vector_emission.retained_world_areas.active(),
                overscale_annotation: !previewed
                    && !self
                        .vector_emission
                        .vector_frame
                        .overscale_annotation
                        .is_empty(),
                coverage_scale_selection: !previewed
                    && self
                        .vector_emission
                        .vector_frame
                        .prepared_coverage
                        .as_ref()
                        .is_some_and(|coverage| coverage.requires_scale_selection_rebuild()),
                dependency_iterations: self
                    .vector_emission
                    .vector_frame
                    .dependency_status
                    .iterations,
                view_dependent_symbols: !previewed
                    && self.vector_emission.vector_frame.view_dependent_symbols,
                view_clipped_patterns: self.vector_emission.vector_frame.view_clipped_patterns,
                source_transform_present: self
                    .vector_emission
                    .vector_frame
                    .geometry_transform
                    .is_some(),
                affine,
            });
            if let Some(cell) = &self.vector_emission.flat_diagnostic {
                cell.borrow_mut().record_reuse(reason);
            }
            if crate::profiler::is_profiling_enabled() {
                self.cpu_profiler.record_navigation_event("attempt");
                if reason.is_empty() {
                    self.cpu_profiler.record_navigation_event("accepted");
                }
                for (bit, name) in [
                    "mode",
                    "dependencies",
                    "view_dependent",
                    "clipped_pattern",
                    "no_source_transform",
                    "no_affine",
                    "invalid_affine",
                    "retained_world_area",
                    "overscale_annotation",
                    "coverage_scale_selection",
                ]
                .iter()
                .enumerate()
                {
                    if reason.0 & (1 << bit) != 0 {
                        self.cpu_profiler.record_navigation_event(name);
                    }
                }
            }
        }
    }
    pub fn set_gpu_view_scaler(&mut self, target: &ferrite_render::Scaler) -> bool {
        if self.vector_emission.gpu_timestamp_requested {
            self.vector_emission.gpu_timestamp_target_camera = target.flat_encoded_identity();
        }
        self.vector_emission.native_route_target_camera = target.flat_encoded_identity();
        if false || self.requires_visibility_rebuild_for_navigation() {
            self.record_flat_reuse(None, false);
            return false;
        }
        let Some(source) = self.vector_emission.vector_frame.geometry_transform else {
            self.record_flat_reuse(None, false);
            return false;
        };
        let Some(view) = ferrite_render::ScreenAffine::between_transform(source, target) else {
            self.record_flat_reuse(None, false);
            return false;
        };
        let pan = [
            view.translation[0] / view.scale[0],
            view.translation[1] / view.scale[1],
        ];
        // Reaching here with preview-only blockers means motion preview is on.
        // Its drift budget bounds how stale the shown placement/coverage gets.
        // The identity affine shows the retained scene at its own camera: exact.
        let identity = view.scale == [1., 1.] && view.translation == [0., 0.];
        let preview = !identity && self.preview_differs_from_exact_navigation();
        let viewport = target.viewport;
        // While a background build is in flight the preview is the only frame
        // available; its install restarts the budget from the newer scene.
        let within_budget = !preview
            || self.scene_build_pending
            || crate::motion_preview::within_drift(
                view.scale,
                view.translation,
                [viewport.x, viewport.y],
                [viewport.width, viewport.height],
            );
        self.record_flat_reuse(
            Some(ferrite_render::flat_reuse_diagnostics::NavigationAffine {
                scale: view.scale,
                pan,
                pivot: [0., 0.],
            }),
            preview && within_budget,
        );
        if !within_budget || !self.accepts_gpu_navigation(view.scale, pan, [0., 0.]) {
            return false;
        }
        self.motion_preview_active = preview;
        self.preview_refresh_due = preview
            && !crate::motion_preview::within_drift_fraction(
                view.scale,
                view.translation,
                [viewport.x, viewport.y],
                [viewport.width, viewport.height],
                crate::motion_preview::PREFETCH_FRACTION,
            );
        self.vector_emission.vector_frame.screen_zoom_scale = view.scale[0];
        self.vector_emission.vector_frame.screen_zoom_scale_y = view.scale[1];
        self.vector_emission.vector_frame.screen_pan_offset = (
            view.translation[0] / view.scale[0],
            view.translation[1] / view.scale[1],
        );
        self.vector_emission.vector_frame.screen_zoom_pivot = (0., 0.);
        self.update_view_uniforms();
        true
    }

    pub fn fast_view_scales(&self) -> (f32, f32) {
        (
            self.vector_emission.vector_frame.screen_zoom_scale,
            self.vector_emission.vector_frame.screen_zoom_scale_y,
        )
    }

    /// Get current GPU zoom scale
    #[inline]
    pub fn gpu_zoom_scale(&self) -> f32 {
        self.vector_emission.vector_frame.screen_zoom_scale
    }

    /// Begin a new frame - clears buffers
    pub fn begin_frame(&mut self) {
        self.begin_frame_ex(false);
    }

    /// Begin a new frame with optional preservation of declutter state
    /// Preservation applies to non-sounding grids; sounding champions use the current view.
    pub fn begin_frame_ex(&mut self, _preserve_declutter: bool) {
        self.state.clear_prebuilt_buffers();
        self.state.sync_scale_factor();
        self.motion_preview_active = false;
        self.vector_scene_epoch.advance();
        self.ui_state.coverage_scale_indication = None;
        self.vector_emission
            .begin_frame(self.exact_line_quad_enabled);
    }

    /// Set current zoom level for symbol filtering
    #[inline]
    pub fn set_zoom_level(&mut self, zoom: f64) {
        self.vector_scene_epoch.advance();
        self.zoom_level = zoom;
    }

    /// Set chart compilation scale (e.g., 22000 for 1:22000)
    #[inline]
    pub fn set_compilation_scale(&mut self, scale: u32) {
        self.vector_scene_epoch.advance();
        self.compilation_scale = scale;
    }

    /// Calculate the current viewing scale based on zoom level
    /// viewing_scale = compilation_scale / zoom_level
    /// Example: At zoom 2.0 with 1:22000 chart -> viewing scale is 1:11000
    #[inline]
    pub fn viewing_scale(&self) -> u32 {
        let mut services = EmissionServices::read_only(
            &self.state,
            &self.pipelines,
            &self.egui.ctx,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission.viewing_scale(&mut services)
    }

    /// Set Natural Earth world map coastlines for background rendering.
    /// Each inner Vec is a line string: list of [longitude, latitude] pairs.
    pub fn set_world_map(&mut self, coastlines: Vec<Vec<[f64; 2]>>) {
        self.scene_generation = self.scene_generation.wrapping_add(1);
        self.vector_scene_epoch.advance();
        tracing::info!("World map loaded: {} coastline segments", coastlines.len());
        self.world_map_coastlines = Arc::new(
            ferrite_render::BackgroundCoastlines::new(coastlines).unwrap_or_else(|e| {
                tracing::warn!("{e}");
                Default::default()
            }),
        );
    }

    /// More detailed reference coastlines for regional views; never used as ENC data.
    pub fn set_world_map_detailed(&mut self, coastlines: Vec<Vec<[f64; 2]>>) {
        self.scene_generation = self.scene_generation.wrapping_add(1);
        self.vector_scene_epoch.advance();
        self.world_map_detailed = Arc::new(
            ferrite_render::BackgroundCoastlines::new(coastlines).unwrap_or_else(|e| {
                tracing::warn!("{e}");
                Default::default()
            }),
        );
    }

    /// Set chart coverage bounding boxes so world map is masked
    /// where chart data exists (opaque background rectangles).
    pub fn set_world_map_chart_boxes(&mut self, boxes: Vec<(f64, f64, f64, f64)>) {
        self.scene_generation = self.scene_generation.wrapping_add(1);
        self.vector_scene_epoch.advance();
        self.vector_emission.vector_frame.world_map_chart_boxes = boxes;
    }

    /// Set the screen pixel width of 360° longitude for wrapping.
    /// Call this after scaler is configured: `renderer.set_lon_wrap_pixels(360.0 * scaler.scale_x as f32)`
    pub fn set_lon_wrap_pixels(&mut self, px: f32) {
        self.vector_emission.vector_frame.lon_wrap_screen_px = px;
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
        self.vector_scene_epoch.advance();
        let mut services = EmissionServices::live(
            &self.state,
            &self.pipelines,
            &mut self.egui,
            &mut self.cpu_profiler,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission
            .add_world_map_lines(&mut services, scaler)
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
        self.vector_emission
            .line_suppression
            .immutable_preparation_bytes()
    }
    pub fn dependency_render_status(&self) -> &DependencyRenderStatus {
        &self.vector_emission.vector_frame.dependency_status
    }

    pub fn add_instructions_with_symbols(
        &mut self,
        context: &mut RenderContext,
        symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
    ) {
        // Legacy callers retain the existing diagnostic at the failure site.
        let _ = self.try_add_instructions_with_symbols(
            context,
            symbol_cache,
            color_profile,
            visible_viewing_groups,
        );
    }

    /// Fallible emission for callers that must reject an unsuccessful preparation.
    /// This mutates renderer state; it is not an atomic publication transaction.
    pub fn try_add_instructions_with_symbols(
        &mut self,
        context: &mut RenderContext,
        symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
    ) -> Result<()> {
        self.add_instructions_with_resources_impl(
            context,
            symbol_cache,
            color_profile,
            visible_viewing_groups,
            None,
        )
    }
    /// Validate owner annotation resources before durable chart publication.
    /// This temporary GPU preparation never replaces the currently displayed frame.
    pub fn preflight_owned_annotation(
        &self,
        context: &RenderContext,
        resources: &mut crate::CellPortrayalResources,
    ) -> Result<()> {
        let Some(prepared) = context
            .prepared_coverage()
            .map_err(|e| WgpuError::Render(e.to_string()))?
        else {
            return Ok(());
        };
        let mut annotations = Vec::new();
        for index in 0..prepared.pass_count() {
            if let Some(annotation) = prepared
                .pass(index)
                .map_err(|e| WgpuError::Render(e.to_string()))?
                .frame()
                .scale_annotations()
            {
                annotations.push(annotation);
            }
        }
        crate::overscale_annotation::OverscaleAnnotation::prepare_owned(
            &annotations,
            &context.scaler,
            resources,
            &self.state,
            &self.pipelines,
            Some(&self.overscale_program_reuse),
        )?;
        Ok(())
    }
    /// Entire canonical instruction stream; only immutable per-cell PC resource routing changes.
    pub fn add_instructions_with_resource_owners(
        &mut self,
        context: &mut RenderContext,
        resources: &mut crate::CellPortrayalResources,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
    ) {
        // Legacy callers retain the existing diagnostic at the failure site.
        let _ = self.try_add_instructions_with_resource_owners(
            context,
            resources,
            visible_viewing_groups,
        );
    }

    /// Propagate resource/visibility failures before a caller decides to publish.
    /// Partial renderer mutations remain possible; prepare on private state for atomicity.
    pub fn try_add_instructions_with_resource_owners(
        &mut self,
        context: &mut RenderContext,
        resources: &mut crate::CellPortrayalResources,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
    ) -> Result<()> {
        if let Err(error) = resources.seal_viewing_groups() {
            self.vector_emission.vector_frame.coverage_failed = true;
            tracing::error!("Mixed-PC group capture rejected: {error}");
            return Err(WgpuError::Render(format!(
                "Mixed-PC group capture rejected: {error}"
            )));
        }
        self.add_instructions_with_resources_impl(
            context,
            None,
            None,
            visible_viewing_groups,
            Some(resources),
        )
    }
    fn add_instructions_with_resources_impl(
        &mut self,
        context: &mut RenderContext,
        symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
        resource_owners: Option<&mut crate::CellPortrayalResources>,
    ) -> Result<()> {
        self.vector_scene_epoch.advance();
        let mut services = EmissionServices::live(
            &self.state,
            &self.pipelines,
            &mut self.egui,
            &mut self.cpu_profiler,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission.add_instructions_with_resources_impl(
            &mut services,
            context,
            symbol_cache,
            color_profile,
            visible_viewing_groups,
            resource_owners,
        )
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
        let mut services = EmissionServices::live(
            &self.state,
            &self.pipelines,
            &mut self.egui,
            &mut self.cpu_profiler,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission
            .ensure_triangulated(&mut services, area, projection)
    }

    /// Pre-computed world→screen transform parameters (avoids per-area scaler lookups)
    #[inline]
    fn scaler_transform(scaler: &ferrite_render::Scaler) -> ferrite_render::FlatTransform {
        scaler.flat_transform()
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
    pub(crate) fn clip_line_segment(
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
        self.vector_emission
            .line_suppression
            .current()
            .and_then(|plan| plan.spans(index))
    }
    pub fn temporal_conditions_changed(&self, context: &RenderContext) -> bool {
        self.vector_emission
            .vector_frame
            .temporal_visibility_mask
            .len()
            != context.instruction_count()
            || context
                .temporal_statuses()
                .into_iter()
                .any(|(index, visible)| {
                    self.vector_emission
                        .vector_frame
                        .temporal_visibility_mask
                        .get(index)
                        != Some(&visible)
                })
    }

    pub fn temporal_visibility_counts(&self) -> (usize, usize) {
        self.vector_emission.vector_frame.temporal_visibility_counts
    }

    /// Optional coarse emitter-order waves; absent means unavailable, not zero cost.
    pub fn emitter_wave_statistics(&self) -> Option<serde_json::Value> {
        self.vector_emission.emitter_wave_work.as_ref().map(|work| serde_json::json!({
            "schema": 2,"stage_names":crate::emitter_wave_diagnostics::NAMES,
            "work": &*work.borrow(),
            "ordered_line_scope":"optional post-admission/owner/suppression scalar helper census; ordinary subset has no dynamic/offset/dash/partial-suppression; coarse source partitions require consecutive original ordinal, plane/priority/cell/resource and original all-pass coverage decision continuity; NOT exact material batches or geometry permission; authored points are not projection operations; actual append/capacity deltas not allocation/RSS/GPU bytes; kind-wave clocks remain inclusive, NOT pure-quad time",
            "scope":"renderer lifetime cumulative HOST, per-emitter-call RAII including early returns; kind waves in original dispatch order BEFORE live admission/owner/errors; projection/clip/append not separated; whole includes pre/dispatch/post, never sum with encompassing prepare or legacy spans; authored points/visited ordinals are logical counters; completed-dispatch append counts are not GPU uploads or allocations",
        }))
    }
    /// Diagnostic-only HOST clocks; nested scopes are not additive GPU time.
    pub fn line_preparation_work(&self) -> Option<crate::LinePreparationWork> {
        self.vector_emission
            .line_preparation_work
            .as_ref()
            .map(|work| work.borrow().clone())
    }

    pub fn line_preparation_statistics(&self) -> Option<serde_json::Value> {
        self.vector_emission.line_preparation_work.as_ref().map(|work| serde_json::json!({
            "stage_names": crate::line_preparation_diagnostics::STAGE_NAMES,
            "work": &*work.borrow(),
            "scope": "renderer lifetime cumulative; delta at callback boundaries; all rebuilds/dependency trials; nested host intervals; projection_clip_mesh includes segment projection, clip and append; no GPU time",
        }))
    }

    /// Used only by the untimed chronological replay before its final gesture.
    pub fn arm_accepted_screen_line_packet(&mut self, replay_pose: usize) {
        self.vector_emission
            .accepted_screen_line_packet
            .arm(replay_pose);
    }
    /// Called by audit_portrayal with its actual serialized ordered IR.
    /// No capture export if the held source token/view is stale or already exported.
    pub fn audit_accepted_screen_line_packet(
        &self,
        directory: &std::path::Path,
        context: &RenderContext,
        instructions: &[u8],
    ) -> std::io::Result<()> {
        if !self
            .vector_emission
            .accepted_screen_line_packet
            .current(context)
        {
            return Ok(());
        }
        if self.vector_emission.vector_frame.coverage_failed
            || self
                .vector_emission
                .vector_frame
                .frame_cpu
                .line_geometry
                .vertex_len()
                != self
                    .vector_emission
                    .accepted_screen_line_packet
                    .final_lengths()[0]
            || self
                .vector_emission
                .vector_frame
                .frame_cpu
                .line_geometry
                .index_len()
                != self
                    .vector_emission
                    .accepted_screen_line_packet
                    .final_lengths()[1]
        {
            return Err(std::io::Error::other(
                "Captured emitter is no longer current ready geometry",
            ));
        }
        use sha2::{Digest, Sha256};
        use std::io::Write;
        let final_path = directory.join("accepted-screen-line-packet.json");
        let temporary = directory.join("accepted-screen-line-packet.partial");
        if final_path.exists() {
            return Err(std::io::Error::other("Refuse packet overwrite"));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| {
            let mut bounded = crate::accepted_screen_line_packet::BoundedWriter::new(file);
            let digest = format!("{:x}", Sha256::digest(instructions));
            self.vector_emission.accepted_screen_line_packet.write(
                &mut bounded,
                &digest,
                context,
            )?;
            bounded.flush()?;
            bounded.sync_all()?;
            drop(bounded);
            std::fs::hard_link(&temporary, &final_path)?;
            std::fs::remove_file(&temporary)?;
            let (line_vertices, line_indices) = self
                .vector_emission
                .vector_frame
                .frame_cpu
                .line_geometry
                .audit_buffers()
                .map_err(std::io::Error::other)?;
            // One chosen pose only; these are original CPU upload payloads, not GPU readbacks.
            for (name, bytes) in [
                (
                    "packet-line-vertices.bin",
                    bytemuck::cast_slice(line_vertices.as_ref()),
                ),
                (
                    "packet-line-indices.bin",
                    bytemuck::cast_slice(line_indices.as_ref()),
                ),
            ] {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.join(name))?;
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join("packet-draw-order-inputs.json"))?;
            let mut writer = crate::accepted_screen_line_packet::BoundedWriter::new(file);
            struct TextOrder<'a>(&'a [GpuChartText]);
            impl serde::Serialize for TextOrder<'_> {
                fn serialize<S: serde::Serializer>(
                    &self,
                    serializer: S,
                ) -> std::result::Result<S::Ok, S::Error> {
                    use serde::ser::SerializeSeq;
                    let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
                    for text in self.0 {
                        seq.serialize_element(&(
                            text.plane,
                            text.priority,
                            text.source,
                            text.wrap_pass,
                            text.scissor,
                            text.index_count,
                        ))?;
                    }
                    seq.end()
                }
            }
            struct RasterOrder<'a>(&'a WgpuRenderer);
            impl serde::Serialize for RasterOrder<'_> {
                fn serialize<S: serde::Serializer>(
                    &self,
                    serializer: S,
                ) -> std::result::Result<S::Ok, S::Error> {
                    use serde::ser::SerializeSeq;
                    let mut seq = serializer.serialize_seq(Some(self.0.raster_layers.len()))?;
                    for (ordinal, layer) in self.0.raster_layers.iter().enumerate() {
                        seq.serialize_element(&(
                            ordinal,
                            layer.draw_order.render_key(),
                            layer.index_count,
                            ferrite_render::raster_groups_visible(
                                &layer.viewing_groups,
                                self.0.raster_enabled_groups.as_ref(),
                            ),
                        ))?;
                    }
                    seq.end()
                }
            }
            #[derive(serde::Serialize)]
            struct DrawOrderInputs<'a, A: serde::Serialize, P: serde::Serialize> {
                scope: &'static str,
                area_ranges: &'a A,
                line_ranges: &'a A,
                symbol_ranges: &'a A,
                pattern_ranges: &'a P,
                text: TextOrder<'a>,
                raster: RasterOrder<'a>,
                world_map_line_vertices: usize,
                world_map_line_indices: usize,
                wrap_pixels_bits: u32,
            }
            serde_json::to_writer(&mut writer,&DrawOrderInputs {
                scope:"actual priority-range inputs, NOT actual GPU draw trace; priority ascending; within each priority wrap0/1/2 each raster/area/pattern/line/symbol; text follows all wraps; annotation/native-host afterwards; coverage bind can reject; keep original encoder until trace qualified",
                area_ranges:&self.vector_emission.vector_frame.frame_cpu.area_priority_ranges,line_ranges:&self.vector_emission.vector_frame.frame_cpu.line_priority_ranges,
                symbol_ranges:&self.vector_emission.vector_frame.frame_cpu.symbol_priority_ranges,pattern_ranges:&self.vector_emission.vector_frame.frame_cpu.pattern_ranges,
                text:TextOrder(&self.chart_text_meshes),raster:RasterOrder(self),
                world_map_line_vertices:self.vector_emission.vector_frame.frame_cpu.world_map_line_vertices.len(),world_map_line_indices:self.vector_emission.vector_frame.frame_cpu.world_map_line_indices.len(),
                wrap_pixels_bits:self.vector_emission.vector_frame.lon_wrap_screen_px.to_bits(),
            }).map_err(std::io::Error::other)?;
            writer.flush()?;
            writer.sync_all()?;
            self.vector_emission
                .accepted_screen_line_packet
                .mark_exported();
            std::fs::write(
                directory.join("accepted-screen-line-packet-statistics.json"),
                serde_json::to_vec(
                    &self
                        .vector_emission
                        .accepted_screen_line_packet
                        .statistics(),
                )
                .map_err(std::io::Error::other)?,
            )?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    pub fn line_visibility_counts(&self) -> (usize, usize) {
        self.vector_emission
            .line_suppression
            .current()
            .map(|p| (p.fully_suppressed.len(), p.partial.len()))
            .unwrap_or_default()
    }

    pub fn prepare_raster_scene_publication(
        &self,
        raster: PreparedRasterPublication,
    ) -> Result<PreparedRasterScenePublication> {
        self.prepare_raster_scene_publication_with_groups(
            raster,
            self.raster_enabled_groups.clone(),
        )
    }

    /// Capture the raster product PC selection with the unpublished inventory.
    /// The caller resolves these handles from the same PC that emitted the rasters.
    pub fn prepare_raster_scene_publication_with_groups(
        &self,
        raster: PreparedRasterPublication,
        enabled_groups: Option<std::collections::HashSet<u32>>,
    ) -> Result<PreparedRasterScenePublication> {
        self.validate_raster_publication(&raster)?;
        Ok(PreparedRasterScenePublication {
            raster,
            enabled_groups,
        })
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
        self.set_raster_enabled_groups(prepared.enabled_groups.as_ref());
        if self.continuous_layer_count == 0 {
            self.reset_pan_offset();
        }
    }

    /// Number of unique symbol ids that failed to render this session.
    pub fn missing_symbol_count(&self) -> usize {
        self.vector_emission.missing_symbol_ids.len()
    }

    /// Number of point instructions emitted without a symbol_ref this session.
    /// A non-zero count indicates a portrayal-rule bug.
    pub fn empty_symbol_ref_count(&self) -> u32 {
        self.vector_emission.empty_symbol_ref_count
    }

    /// Exact font, rotation, wrapping and collision policy shared with drawing.
    fn layout_chart_text(
        &self,
        shapes: Vec<ChartTextShape>,
        capture_shapes: bool,
    ) -> (Vec<ChartTextShape>, Vec<bool>) {
        self.layout_chart_text_with_owner(
            self.vector_emission.referenced_chart_owner.as_ref(),
            shapes,
            capture_shapes,
        )
    }
    fn layout_chart_text_with_owner(
        &self,
        owner: Option<&crate::referenced_chart_owner::ReferencedChartOwner>,
        shapes: Vec<ChartTextShape>,
        capture_shapes: bool,
    ) -> (Vec<ChartTextShape>, Vec<bool>) {
        let mut services = EmissionServices::read_only(
            &self.state,
            &self.pipelines,
            &self.egui.ctx,
            &self.overscale_program_reuse,
            self.symbol_scale,
            self.show_soundings,
            self.animation_mode,
            self.ui_state.settings.show_shallow_pattern,
            &self.world_map_coastlines,
            &self.world_map_detailed,
            self.background_color,
        );
        self.vector_emission.layout_chart_text_with_owner(
            &mut services,
            owner,
            shapes,
            capture_shapes,
        )
    }
    /// Shared text/selection overlay; optionally includes application panels.
    fn prepare_overlay(&mut self, include_panels: bool) -> Result<egui::FullOutput> {
        if let Some(owner) = self.vector_emission.referenced_chart_owner.as_ref() {
            owner.begin_display(
                [self.state.size.width, self.state.size.height],
                self.state.window.scale_factor() as f32,
                self.egui.ctx.pixels_per_point(),
            )?;
        }
        self.chart_text_shapes.clear();
        // Begin egui frame
        self.egui.begin_frame(&self.state.window);

        // Read exactly the committed draw/pick CoverageFrame. Resized or affine
        // preview geometry is not a new coverage owner/reference; withhold its
        // stale annotation until an authoritative frame has been prepared.
        let size = self.state.window.inner_size();
        let physical_extent = [size.width, size.height];
        let authoritative = !self.vector_emission.vector_frame.coverage_failed
            && self.vector_emission.vector_frame.screen_pan_offset == (0.0, 0.0)
            && self.vector_emission.vector_frame.screen_zoom_scale == 1.0
            && self.vector_emission.vector_frame.screen_zoom_scale_y == 1.0;
        self.ui_state.coverage_scale_indication = if let (true, Some(viewport)) = (
            authoritative,
            self.vector_emission.vector_frame.chart_geometry_viewport,
        ) {
            self.vector_emission
                .vector_frame
                .prepared_coverage
                .as_ref()
                .and_then(|prepared| prepared.pass(0).ok())
                .and_then(|pass| pass.frame().scale_annotations())
                .filter(|annotation| annotation.physical_extent() == physical_extent)
                .filter(|annotation| {
                    annotation.reference_point
                        == [
                            viewport.x as f64 + viewport.width as f64 * 0.5,
                            viewport.y as f64 + viewport.height as f64 * 0.5,
                        ]
                })
                .and_then(|annotation| {
                    annotation.reference().map(|reference| {
                        crate::egui_integration::CoverageScaleIndication {
                            viewing_denominator: annotation.viewing_denominator,
                            overscale_factor: reference.state.factor,
                            physical_viewport: viewport,
                            sclbr: self
                                .vector_emission
                                .vector_frame
                                .coverage_scale_colours
                                .get(&reference.dataset_id)
                                .copied()
                                .or(self.vector_emission.vector_frame.coverage_scale_colour),
                        }
                    })
                })
        } else {
            None
        };

        // Adapter is the actual selected device, not a guessed platform label.
        if include_panels && self.ui_state.debug_mode {
            self.ui_state
                .debug_gpu
                .adapter
                .clone_from(&self.state.gpu_name);
            let backend = match self.state.adapter_backend {
                wgpu::Backend::Metal => "Metal",
                wgpu::Backend::Vulkan => "Vulkan",
                wgpu::Backend::Dx12 => "DirectX 12",
                wgpu::Backend::Gl => "OpenGL",
                wgpu::Backend::BrowserWebGpu => "WebGPU",
                _ => "Other",
            };
            if self.ui_state.debug_gpu.backend != backend {
                self.ui_state.debug_gpu.backend.clear();
                self.ui_state.debug_gpu.backend.push_str(backend);
            }
            self.gpu_profiler
                .update_debug_stats(&mut self.ui_state.debug_gpu);
            self.cpu_profiler
                .update_debug_stats(&mut self.ui_state.debug_cpu_frames);
        }

        // Draw egui UI
        if include_panels {
            self.egui.draw_ui(&mut self.ui_state);
        }

        // Selection is an application overlay, separate from IHO portrayal symbols.
        if let Some([x, y]) = self.vector_emission.vector_frame.selection_anchor {
            let (pivot_x, pivot_y) = self.vector_emission.vector_frame.screen_zoom_pivot;
            let ppp = self.egui.ctx.pixels_per_point();
            let pos = egui::pos2(
                ((x + self.vector_emission.vector_frame.screen_pan_offset.0 - pivot_x)
                    * self.vector_emission.vector_frame.screen_zoom_scale
                    + pivot_x)
                    / ppp,
                ((y + self.vector_emission.vector_frame.screen_pan_offset.1 - pivot_y)
                    * self.vector_emission.vector_frame.screen_zoom_scale_y
                    + pivot_y)
                    / ppp,
            );
            let (x, y, w, h) = self.ui_state.chart_area;
            let clip = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h));
            let painter = self
                .egui
                .ctx
                .layer_painter(egui::LayerId::background())
                .with_clip_rect(clip);
            for path in &self.vector_emission.vector_frame.selection_screen_geometry {
                let points: Vec<_> = path
                    .iter()
                    .map(|p| {
                        egui::pos2(
                            ((p[0] + self.vector_emission.vector_frame.screen_pan_offset.0
                                - pivot_x)
                                * self.vector_emission.vector_frame.screen_zoom_scale
                                + pivot_x)
                                / ppp,
                            ((p[1] + self.vector_emission.vector_frame.screen_pan_offset.1
                                - pivot_y)
                                * self.vector_emission.vector_frame.screen_zoom_scale_y
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
        let output = self.egui.end_frame(&self.state.window);
        if let Some(owner) = self.vector_emission.referenced_chart_owner.as_mut() {
            owner.end_display()?;
        }
        Ok(output)
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
    // Whole-frame admission BEFORE any compact upload. Original buffers remain fallback.
    fn prepare_exact_line_buffers(&mut self) -> bool {
        self.exact_line_quad_work.attempts = self.exact_line_quad_work.attempts.saturating_add(1);
        let owned;
        let packed: &[crate::exact_line_quad::ExactLineQuad] = if let Some(primary) = self
            .vector_emission
            .vector_frame
            .frame_cpu
            .line_geometry
            .packed()
        {
            if !crate::exact_line_quad::layout_admitted(
                self.vector_emission
                    .vector_frame
                    .frame_cpu
                    .line_geometry
                    .vertex_len(),
                self.vector_emission
                    .vector_frame
                    .frame_cpu
                    .line_geometry
                    .index_len(),
                self.vector_emission
                    .vector_frame
                    .frame_cpu
                    .line_priority_ranges
                    .iter()
                    .map(|r| (r.2, r.3)),
            ) || primary.is_empty()
            {
                self.exact_line_quad_work.declines =
                    self.exact_line_quad_work.declines.saturating_add(1);
                return false;
            }
            self.primary_line_uploads = self.primary_line_uploads.saturating_add(1);
            primary
        } else {
            self.primary_line_repacks = self.primary_line_repacks.saturating_add(1);
            let (vertices, indices) = self
                .vector_emission
                .vector_frame
                .frame_cpu
                .line_geometry
                .legacy()
                .expect("legacy backend");
            let Some(result) = crate::exact_line_quad::pack_legacy(
                vertices,
                indices,
                self.vector_emission
                    .vector_frame
                    .frame_cpu
                    .line_priority_ranges
                    .iter()
                    .map(|r| (r.2, r.3)),
            )
            .filter(|p| !p.is_empty()) else {
                self.exact_line_quad_work.declines =
                    self.exact_line_quad_work.declines.saturating_add(1);
                return false;
            };
            owned = result;
            &owned
        };
        let cold_index = self.exact_line_quad_pipelines.is_none();
        if cold_index {
            match crate::exact_line_quad::Pipelines::new(
                &self.state,
                &self.pipelines.view_bind_group_layout,
            ) {
                Ok(pipelines) => self.exact_line_quad_pipelines = Some(pipelines),
                Err(error) => {
                    tracing::warn!("Exact line representation declined: {error}");
                    self.exact_line_quad_work.declines =
                        self.exact_line_quad_work.declines.saturating_add(1);
                    return false;
                }
            }
            self.exact_line_quad_work.shared_index_uploads = self
                .exact_line_quad_work
                .shared_index_uploads
                .saturating_add(1);
            if let Some(cell) = &self.vector_emission.flat_diagnostic {
                let mut row = cell.borrow_mut();
                row.work.buffer_upload_calls = row.work.buffer_upload_calls.saturating_add(1);
                row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(24);
            }
        }
        let Some(pipelines) = self.exact_line_quad_pipelines.as_mut() else {
            return false;
        };
        if let Err(error) = pipelines.prepare_masked(
            &self.state,
            &self.pipelines.view_bind_group_layout,
            self.vector_emission.coverage_pipelines.as_ref(),
        ) {
            tracing::warn!("Exact masked line representation declined: {error}");
            self.exact_line_quad_work.declines =
                self.exact_line_quad_work.declines.saturating_add(1);
            return false;
        }
        let bytes = bytemuck::cast_slice::<_, u8>(packed).len() as u64;
        self.cached_line_vb = Some(
            self.state
                .create_vertex_buffer(packed, "exact-line-quad-instances"),
        );
        self.cached_line_ib = Some(pipelines.indices.clone());
        self.cached_line_index_count = self
            .vector_emission
            .vector_frame
            .frame_cpu
            .line_geometry
            .index_len() as u32;
        self.cached_line_compact = true;
        self.exact_line_cpu_owner = Some(Arc::clone(
            &self.vector_emission.vector_frame.frame_cpu.owner,
        ));
        self.exact_line_quad_work.admitted = self.exact_line_quad_work.admitted.saturating_add(1);
        self.exact_line_quad_work.legacy_payload_bytes = self
            .exact_line_quad_work
            .legacy_payload_bytes
            .saturating_add(
                (self
                    .vector_emission
                    .vector_frame
                    .frame_cpu
                    .line_geometry
                    .vertex_len()
                    * std::mem::size_of::<LineVertex>()
                    + self
                        .vector_emission
                        .vector_frame
                        .frame_cpu
                        .line_geometry
                        .index_len()
                        * std::mem::size_of::<u32>()) as u64,
            );
        self.exact_line_quad_work.packed_upload_bytes = self
            .exact_line_quad_work
            .packed_upload_bytes
            .saturating_add(bytes);
        self.exact_line_quad_work.current_gpu_payload_bytes = bytes;
        if let Some(cell) = &self.vector_emission.flat_diagnostic {
            let mut row = cell.borrow_mut();
            row.work.buffer_upload_calls = row.work.buffer_upload_calls.saturating_add(1);
            row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(bytes);
        }
        true
    }

    fn prepare_geometry_buffers(&mut self) -> crate::Result<()> {
        self.vector_scene_epoch.advance();
        // Original camera reuse/dirty semantics remain authoritative. This adds only
        // representation/layout retirement, never permission or scene visibility.
        if self.cached_line_compact
            && (!self.exact_line_cpu_owner.as_ref().is_some_and(|owner| {
                Arc::ptr_eq(owner, &self.vector_emission.vector_frame.frame_cpu.owner)
            }) || self
                .vector_emission
                .vector_frame
                .frame_cpu
                .line_geometry
                .index_len()
                != self.cached_line_index_count as usize
                || !crate::exact_line_quad::layout_admitted(
                    self.vector_emission
                        .vector_frame
                        .frame_cpu
                        .line_geometry
                        .vertex_len(),
                    self.vector_emission
                        .vector_frame
                        .frame_cpu
                        .line_geometry
                        .index_len(),
                    self.vector_emission
                        .vector_frame
                        .frame_cpu
                        .line_priority_ranges
                        .iter()
                        .map(|r| (r.2, r.3)),
                )
                || self.exact_line_quad_pipelines.as_ref().is_none_or(|p| {
                    !p.matches_coverage(self.vector_emission.coverage_pipelines.as_ref())
                }))
        {
            self.vector_emission.gpu_buffers_dirty = true;
        }
        let _flat_upload_span =
            self.flat_span(ferrite_render::flat_reuse_diagnostics::FlatFrameStage::BufferUpload);
        if self.vector_emission.gpu_buffers_dirty {
            let candidate_output = self.vector_emission.retained_world_areas.prepare(
                &self.state.device,
                &self.state.queue,
                &self.vector_emission.vector_frame.frame_cpu.area_vertices,
                self.vector_emission.vector_frame.screen_pan_offset == (0.0, 0.0)
                    && self.vector_emission.vector_frame.screen_zoom_scale == 1.0
                    && self.vector_emission.vector_frame.screen_zoom_scale_y == 1.0,
            );
            if self.vector_emission.retained_world_areas.active() {
                if let Some(output) = candidate_output {
                    self.cached_area_vb = Some(output);
                }
            } else {
                self.cached_area_vb = if !self
                    .vector_emission
                    .vector_frame
                    .frame_cpu
                    .area_vertices
                    .is_empty()
                {
                    Some({
                        if let Some(cell) = &self.vector_emission.flat_diagnostic {
                            let mut row = cell.borrow_mut();
                            row.work.buffer_upload_calls =
                                row.work.buffer_upload_calls.saturating_add(1);
                            row.work.buffer_upload_bytes =
                                row.work.buffer_upload_bytes.saturating_add(
                                    bytemuck::cast_slice::<_, u8>(
                                        &self.vector_emission.vector_frame.frame_cpu.area_vertices,
                                    )
                                    .len() as u64,
                                );
                        }
                        self.state.create_vertex_buffer(
                            &self.vector_emission.vector_frame.frame_cpu.area_vertices,
                            "area_vertices",
                        )
                    })
                } else {
                    None
                };
            }
            let area_index_hit = self.area_index_upload_reuse.reuse(
                &self.state.device,
                &self.vector_emission.vector_frame.frame_cpu.area_indices,
                self.cached_area_ib.is_some(),
                self.cached_area_index_count,
            );
            if !area_index_hit {
                // Invalidate before every normal replacement/clear. Exact bytes are
                // never used as owner/scale/coverage/permission admission.
                self.area_index_upload_reuse.invalidate();
                self.cached_area_ib = if !self
                    .vector_emission
                    .vector_frame
                    .frame_cpu
                    .area_indices
                    .is_empty()
                {
                    self.cached_area_index_count = self
                        .vector_emission
                        .vector_frame
                        .frame_cpu
                        .area_indices
                        .len() as u32;
                    Some({
                        if let Some(cell) = &self.vector_emission.flat_diagnostic {
                            let mut row = cell.borrow_mut();
                            row.work.buffer_upload_calls =
                                row.work.buffer_upload_calls.saturating_add(1);
                            row.work.buffer_upload_bytes =
                                row.work.buffer_upload_bytes.saturating_add(
                                    bytemuck::cast_slice::<_, u8>(
                                        &self.vector_emission.vector_frame.frame_cpu.area_indices,
                                    )
                                    .len() as u64,
                                );
                        }
                        self.state.create_index_buffer(
                            &self.vector_emission.vector_frame.frame_cpu.area_indices,
                            "area_indices",
                        )
                    })
                } else {
                    self.cached_area_index_count = 0;
                    None
                };
                if self.cached_area_ib.is_some() {
                    self.area_index_upload_reuse.uploaded(
                        self.state.device.clone(),
                        &self.vector_emission.vector_frame.frame_cpu.area_indices,
                    );
                }
            }

            self.cached_line_compact = false;
            self.exact_line_cpu_owner = None;
            self.exact_line_quad_work.current_gpu_payload_bytes = 0;
            if !self.exact_line_quad_enabled || !self.prepare_exact_line_buffers() {
                self.vector_emission
                    .vector_frame
                    .frame_cpu
                    .line_geometry
                    .materialize()
                    .map_err(|e| crate::WgpuError::Render(e.into()))?;
                self.cached_line_compact = false;
                self.exact_line_cpu_owner = None;
                if self.vector_emission.vector_frame.scene_bounds.is_none() {
                    self.vector_emission.vector_frame.compute_scene_bounds();
                }
                let (line_vertices, line_indices) = self
                    .vector_emission
                    .vector_frame
                    .frame_cpu
                    .line_geometry
                    .legacy()
                    .expect("materialized backend");
                self.cached_line_vb = if !line_vertices.is_empty() {
                    Some({
                        if let Some(cell) = &self.vector_emission.flat_diagnostic {
                            let mut row = cell.borrow_mut();
                            row.work.buffer_upload_calls =
                                row.work.buffer_upload_calls.saturating_add(1);
                            row.work.buffer_upload_bytes =
                                row.work.buffer_upload_bytes.saturating_add(
                                    bytemuck::cast_slice::<_, u8>(line_vertices).len() as u64,
                                );
                        }
                        self.state
                            .create_vertex_buffer(line_vertices, "line_vertices")
                    })
                } else {
                    None
                };
                let topology_buffer = self.immutable_line_topology.get(
                    line_indices.len(),
                    self.vector_emission
                        .vector_frame
                        .frame_cpu
                        .line_geometry
                        .original_quad_topology(),
                );
                self.cached_line_ib = if let Some(buffer) = topology_buffer {
                    self.cached_line_index_count =
                        u32::try_from(line_indices.len()).map_err(|_| {
                            crate::WgpuError::Render("Line index count overflow".into())
                        })?;
                    Some(buffer)
                } else if !line_indices.is_empty() {
                    self.cached_line_index_count = self
                        .vector_emission
                        .vector_frame
                        .frame_cpu
                        .line_geometry
                        .index_len() as u32;
                    Some({
                        if let Some(cell) = &self.vector_emission.flat_diagnostic {
                            let mut row = cell.borrow_mut();
                            row.work.buffer_upload_calls =
                                row.work.buffer_upload_calls.saturating_add(1);
                            row.work.buffer_upload_bytes =
                                row.work.buffer_upload_bytes.saturating_add(
                                    bytemuck::cast_slice::<_, u8>(line_indices).len() as u64,
                                );
                        }
                        self.state.create_index_buffer(line_indices, "line_indices")
                    })
                } else {
                    self.cached_line_index_count = 0;
                    None
                };
            }

            self.cached_pattern_vb = if !self
                .vector_emission
                .vector_frame
                .frame_cpu
                .pattern_vertices
                .is_empty()
            {
                Some({
                    if let Some(cell) = &self.vector_emission.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(
                                &self.vector_emission.vector_frame.frame_cpu.pattern_vertices,
                            )
                            .len() as u64,
                        );
                    }
                    self.state.create_vertex_buffer(
                        &self.vector_emission.vector_frame.frame_cpu.pattern_vertices,
                        "pattern_vertices",
                    )
                })
            } else {
                None
            };
            self.cached_pattern_ib = if !self
                .vector_emission
                .vector_frame
                .frame_cpu
                .pattern_indices
                .is_empty()
            {
                self.cached_pattern_index_count = self
                    .vector_emission
                    .vector_frame
                    .frame_cpu
                    .pattern_indices
                    .len() as u32;
                Some({
                    if let Some(cell) = &self.vector_emission.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(
                                &self.vector_emission.vector_frame.frame_cpu.pattern_indices,
                            )
                            .len() as u64,
                        );
                    }
                    self.state.create_index_buffer(
                        &self.vector_emission.vector_frame.frame_cpu.pattern_indices,
                        "pattern_indices",
                    )
                })
            } else {
                self.cached_pattern_index_count = 0;
                None
            };

            // World map separate GPU buffers
            self.cached_wm_line_vb = if !self
                .vector_emission
                .vector_frame
                .frame_cpu
                .world_map_line_vertices
                .is_empty()
            {
                Some({
                    if let Some(cell) = &self.vector_emission.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(
                                &self
                                    .vector_emission
                                    .vector_frame
                                    .frame_cpu
                                    .world_map_line_vertices,
                            )
                            .len() as u64,
                        );
                    }
                    self.state.create_vertex_buffer(
                        &self
                            .vector_emission
                            .vector_frame
                            .frame_cpu
                            .world_map_line_vertices,
                        "wm_line_vb",
                    )
                })
            } else {
                None
            };
            self.cached_wm_line_ib = if !self
                .vector_emission
                .vector_frame
                .frame_cpu
                .world_map_line_indices
                .is_empty()
            {
                Some({
                    if let Some(cell) = &self.vector_emission.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(
                                &self
                                    .vector_emission
                                    .vector_frame
                                    .frame_cpu
                                    .world_map_line_indices,
                            )
                            .len() as u64,
                        );
                    }
                    self.state.create_index_buffer(
                        &self
                            .vector_emission
                            .vector_frame
                            .frame_cpu
                            .world_map_line_indices,
                        "wm_line_ib",
                    )
                })
            } else {
                None
            };
            self.cached_wm_mask_vb = if !self
                .vector_emission
                .vector_frame
                .frame_cpu
                .world_map_mask_vertices
                .is_empty()
            {
                Some({
                    if let Some(cell) = &self.vector_emission.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(
                                &self
                                    .vector_emission
                                    .vector_frame
                                    .frame_cpu
                                    .world_map_mask_vertices,
                            )
                            .len() as u64,
                        );
                    }
                    self.state.create_vertex_buffer(
                        &self
                            .vector_emission
                            .vector_frame
                            .frame_cpu
                            .world_map_mask_vertices,
                        "wm_mask_vb",
                    )
                })
            } else {
                None
            };
            self.cached_wm_mask_ib = if !self
                .vector_emission
                .vector_frame
                .frame_cpu
                .world_map_mask_indices
                .is_empty()
            {
                Some({
                    if let Some(cell) = &self.vector_emission.flat_diagnostic {
                        let mut row = cell.borrow_mut();
                        row.work.buffer_upload_calls =
                            row.work.buffer_upload_calls.saturating_add(1);
                        row.work.buffer_upload_bytes = row.work.buffer_upload_bytes.saturating_add(
                            bytemuck::cast_slice::<_, u8>(
                                &self
                                    .vector_emission
                                    .vector_frame
                                    .frame_cpu
                                    .world_map_mask_indices,
                            )
                            .len() as u64,
                        );
                    }
                    self.state.create_index_buffer(
                        &self
                            .vector_emission
                            .vector_frame
                            .frame_cpu
                            .world_map_mask_indices,
                        "wm_mask_ib",
                    )
                })
            } else {
                None
            };

            self.vector_emission.gpu_buffers_dirty = false;
            // Unclaimed prebuilt buffers (e.g. reused topology) are not kept.
            self.state.clear_prebuilt_buffers();
        }
        // Pre-build all symbol GPU buffers before the render pass.
        // This avoids mutable self borrows inside the render pass where view bind groups
        // are held as immutable references (for longitude wrapping multi-pass rendering).
        {
            // Copy one tuple before mutable packing. Packing changes only the
            // packed output arrays; it never changes symbol_priority_ranges.
            let range_count = self
                .vector_emission
                .vector_frame
                .frame_cpu
                .symbol_priority_ranges
                .len();
            // Bound the transient lookup; oversized charts use the original scan.
            let mut symbol_keys = (self.draw_range_index_enabled
                && !false
                && self.vector_emission.cached_symbol_buffers.len() <= 32_768
                && range_count <= 32_768)
                .then(|| {
                    self.vector_emission
                        .cached_symbol_buffers
                        .iter()
                        .map(|(p, q, a, b, _, _, _)| (*p, *q, *a, *b))
                        .collect::<FxHashSet<_>>()
                });
            for range_index in 0..range_count {
                let (pl, pri, start, end, _) = self
                    .vector_emission
                    .vector_frame
                    .frame_cpu
                    .symbol_priority_ranges[range_index];
                if end <= start {
                    continue;
                }
                let already_cached = match &symbol_keys {
                    Some(keys) => keys.contains(&(pl, pri, start, end)),
                    None => self.vector_emission.cached_symbol_buffers.iter().any(
                        |(cp, cpr, cs, ce, _, _, _)| {
                            *cp == pl && *cpr == pri && *cs == start && *ce == end
                        },
                    ),
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
                            if let Some(cell) = &self.vector_emission.flat_diagnostic {
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
                            if let Some(cell) = &self.vector_emission.flat_diagnostic {
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
                self.vector_emission
                    .cached_symbol_buffers
                    .push((pl, pri, start, end, sym_vb, sym_ib, ranges));
                if let Some(keys) = &mut symbol_keys {
                    keys.insert((pl, pri, start, end));
                }
            }
        }
        Ok(())
    }

    /// Tessellate each ordered glyph group into its own immutable GPU buffers.
    /// The egui atlas is shared; no per-priority atlas copies or repeated job scans.
    fn prepare_chart_text(&mut self, output: &mut egui::FullOutput) -> Result<()> {
        self.vector_scene_epoch.advance();
        if let Some(owner) = self.vector_emission.referenced_chart_owner.as_mut() {
            owner.upload(&self.state.device, &self.state.queue)?;
        } else {
            self.egui
                .prepare_chart_atlas(&self.state.device, &self.state.queue, output);
        }
        let cache_environment = immutable_text_preparation::Environment::new(
            output.pixels_per_point,
            [self.state.size.width, self.state.size.height],
        );
        if self.immutable_text_preparation.enabled() {
            if let Some(owner) = self.vector_emission.referenced_chart_owner.as_ref() {
                if self.immutable_text_preparation.matches(
                    owner,
                    cache_environment,
                    &self.chart_text_shapes,
                ) && self
                    .immutable_text_preparation
                    .replay(owner, &mut self.chart_text_meshes)?
                {
                    self.chart_text_shapes.clear();
                    return Ok(());
                }
            }
        }
        self.immutable_text_preparation.clear();
        let pending_text_key = if self.immutable_text_preparation.enabled() {
            self.vector_emission
                .referenced_chart_owner
                .as_ref()
                .and_then(|owner| {
                    immutable_text_preparation::Key::capture(
                        owner,
                        cache_environment,
                        &self.chart_text_shapes,
                    )
                })
        } else {
            None
        };
        let mut pending_text_lookups = pending_text_key.as_ref().map(|_| Vec::new());
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
            let font_context = self
                .vector_emission
                .referenced_chart_owner
                .as_ref()
                .map_or(&self.egui.ctx, |owner| &owner.context);
            for job in font_context.tessellate(shapes, ppp) {
                if let egui::epaint::Primitive::Mesh(mesh) = job.primitive {
                    if mesh.indices.is_empty() {
                        continue;
                    }
                    immutable_text_preparation::record_lookup(
                        &mut pending_text_lookups,
                        mesh.texture_id,
                    );
                    let bind_group =
                        if let Some(owner) = self.vector_emission.referenced_chart_owner.as_ref() {
                            owner.bind_group(mesh.texture_id)?
                        } else {
                            self.egui
                                .chart_atlas_bind_group(mesh.texture_id)
                                .ok_or_else(|| {
                                    WgpuError::Render(format!(
                                        "Chart font atlas {:?} missing",
                                        mesh.texture_id
                                    ))
                                })?
                        };
                    let Some(scissor) = chart_text_scissor(
                        job.clip_rect,
                        ppp,
                        [self.state.size.width, self.state.size.height],
                    ) else {
                        continue;
                    };
                    let vertices = chart_text_vertices(&mesh, ppp);
                    let (vertex_buffer, index_buffer) = self.chart_text_buffers.upload(
                        &self.state.device,
                        &self.state.queue,
                        &vertices,
                        &mesh.indices,
                    );
                    self.chart_text_meshes.push(GpuChartText {
                        texture_id: mesh.texture_id,
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
        self.immutable_text_preparation.store(
            pending_text_key,
            pending_text_lookups,
            &self.chart_text_meshes,
        );
        Ok(())
    }

    pub fn chart_text_buffer_statistics(&self) -> serde_json::Value {
        serde_json::json!({"immutable_preparation":self.immutable_text_preparation.statistics(),"allocations":self.chart_text_buffers.allocations,"writes":self.chart_text_buffers.writes,"retained_bytes":self.chart_text_buffers.retained_bytes,"budget_bytes":ChartTextBufferPool::BUDGET,"slots":self.chart_text_buffers.slots.len()})
    }

    /// Default 4x matches chart antialiasing; 1x permits controlled sampling audits.
    /// Disable retained area meshes for constrained hosts or differential audits.
    /// The next preparation releases cached areas and executes identical cold draping.
    /// Disable only immutable source-curve preparation for controlled differential tests.
    /// Same-binary diagnostic control; only broad-phase selection changes.
    pub fn set_spatial_hierarchy_enabled(&mut self, enabled: bool) {
        self.invalidate_scene_builder();
        self.vector_emission.spatial_hierarchy_enabled = enabled;
    }

    /// Encode the same ordered geometry pass for every render target.
    fn encode_chart(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        resolve_target: Option<&wgpu::TextureView>,
        gpu_query: Option<&wgpu_profiler::GpuProfilerQuery>,
        batch_timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        self.displayed_vector_draw_scene().encode(
            encoder,
            target_view,
            resolve_target,
            gpu_query,
            batch_timestamp_writes,
        );
    }

    /// Private prepare/validate/swap; no external unvalidated commit entry point.
    pub fn sync_native_route_gpu(
        &mut self,
        packet: Option<Arc<crate::PreparedNativeRouteOverlay>>,
        scaler: &ferrite_render::Scaler,
    ) -> std::result::Result<(), String> {
        let mut next_epoch = self.vector_scene_epoch.clone();
        next_epoch.advance();
        let Some(packet) = packet else {
            self.vector_scene_epoch = next_epoch;
            self.native_route_last_encoded.set(false);
            self.native_route_gpu = None;
            self.vector_emission.native_route_target_camera = None;
            return Ok(());
        };
        let surface = [self.state.size.width, self.state.size.height];
        if let Some(current) = &self.native_route_gpu {
            if Arc::ptr_eq(current.packet(), &packet)
                && current.gpu_identity_matches(&self.state, &self.pipelines)
                && current
                    .validate(
                        &self.native_route_gpu_owner,
                        packet.resource_owner(),
                        packet.route_revision(),
                        scaler,
                        scaler.pixels_per_mm(),
                        surface,
                    )
                    .is_ok()
            {
                self.vector_scene_epoch = next_epoch;
                self.native_route_last_encoded.set(false);
                self.vector_emission.native_route_target_camera = scaler.flat_encoded_identity();
                return Ok(());
            }
        }
        let result = crate::PreparedNativeRouteGpuPublication::prepare(
            &self.state,
            &self.pipelines,
            self.native_route_gpu_owner.clone(),
            packet.clone(),
            self.native_route_gpu.as_ref(),
            scaler,
        )
        .and_then(|candidate| {
            candidate.validate(
                &self.native_route_gpu_owner,
                packet.resource_owner(),
                packet.route_revision(),
                scaler,
                scaler.pixels_per_mm(),
                surface,
            )?;
            Ok(candidate)
        });
        match result {
            Ok(candidate) => {
                self.vector_scene_epoch = next_epoch;
                self.native_route_last_encoded.set(false);
                self.vector_emission.native_route_target_camera = scaler.flat_encoded_identity();
                self.native_route_gpu = Some(candidate);
                Ok(())
            }
            Err(error) => {
                // Preserve complete old resources for rollback, but never draw
                // or offer picks under an unprepared current camera.
                // A same-camera edit failure retains the previous publication.
                // A navigation failure must still block the stale route frame:
                // the encoder checks target identity, not a borrowed live Scaler.
                if self.vector_emission.native_route_target_camera != scaler.flat_encoded_identity()
                {
                    self.vector_emission.native_route_target_camera = None;
                    self.native_route_last_encoded.set(false);
                }
                Err(error)
            }
        }
    }
    pub fn invalidate_native_route_gpu(&mut self) {
        self.vector_emission.native_route_target_camera = None;
        self.native_route_last_encoded.set(false);
    }
    pub fn native_route_encoded_in_last_pass(&self) -> bool {
        self.native_route_last_encoded.get()
    }
    pub fn audit_native_route_cpu_inputs(
        &self,
        directory: &std::path::Path,
    ) -> std::io::Result<()> {
        if let Some(publication) = &self.native_route_gpu {
            publication.audit_cpu_inputs(directory)
        } else {
            Err(std::io::Error::other("No native route publication"))
        }
    }

    pub fn audit_native_route_gpu_buffers(&self, directory: &std::path::Path) -> Result<()> {
        if !crate::background_test::enabled()
            || self.window().is_visible() != Some(false)
            || self.window().has_focus()
        {
            return Err(WgpuError::Render(
                "Native route readback requires hidden unfocused mode".into(),
            ));
        }
        let publication = self
            .native_route_gpu
            .as_ref()
            .ok_or_else(|| WgpuError::Render("No native route publication".into()))?;
        publication
            .audit_gpu_buffers(&self.state, directory)
            .map_err(WgpuError::Render)
    }

    /// GPU chart execution only; this excludes UI, acquisition and presentation.
    pub fn gpu_timing_audit(&self) -> serde_json::Value {
        self.gpu_profiler.audit_value()
    }

    /// Explicit hidden audit begins a bounded 64-frame GPU-only display-pass batch.
    pub fn begin_gpu_frame_timestamp_batch(&mut self) -> Result<()> {
        if !self.vector_emission.gpu_timestamp_requested {
            return Err(WgpuError::Render("gpu_timestamp_flag_not_exact_1".into()));
        }
        if !crate::background_test::enabled()
            || self.window().is_visible() != Some(false)
            || self.window().has_focus()
        {
            return Err(WgpuError::Render(
                "GPU timestamp batch requires hidden unfocused audit".into(),
            ));
        }
        if self.gpu_profiler.is_enabled() {
            return Err(WgpuError::Render("legacy_gpu_profiler_active".into()));
        }
        if !self
            .state
            .adapter_features
            .contains(wgpu::Features::TIMESTAMP_QUERY)
        {
            return Err(WgpuError::Render(
                "adapter_timestamp_query_unsupported".into(),
            ));
        }
        if self.gpu_timestamp_batch.is_some() {
            return Err(WgpuError::Render("timestamp_batch_already_owned".into()));
        }
        self.gpu_timestamp_batch = Some(
            crate::gpu_frame_timestamp::Batch::new(
                &self.state.device,
                &self.state.queue,
                self.state.config.format,
            )
            .map_err(WgpuError::Render)?,
        );
        Ok(())
    }
    /// Called only AFTER the measured frame window; adds one resolve/readback submission.
    pub fn finish_gpu_frame_timestamp_batch(&mut self) -> Result<serde_json::Value> {
        if !crate::background_test::enabled()
            || self.window().is_visible() != Some(false)
            || self.window().has_focus()
        {
            return Err(WgpuError::Render(
                "GPU timestamp readback requires hidden unfocused audit".into(),
            ));
        }
        let Some(batch) = self.gpu_timestamp_batch.take() else {
            return Ok(
                serde_json::json!({"gpu_chart_ms":null,"unavailable_reason":if !self.vector_emission.gpu_timestamp_requested {"flag_disabled"}else if !self.state.adapter_features.contains(wgpu::Features::TIMESTAMP_QUERY){"adapter_timestamp_query_unsupported"}else{"batch_not_started"},"adapter_backend":format!("{:?}",self.state.adapter_backend)}),
            );
        };
        let mut result = batch
            .read_after_batch(&self.state.device)
            .map_err(WgpuError::Render)?;
        // This allocation occurs only in explicit post-window report generation.
        result["adapter_backend"] =
            serde_json::Value::String(format!("{:?}", self.state.adapter_backend));
        result["gpu_name"] = serde_json::Value::String(self.state.gpu_name.clone());
        result["device_timestamp_feature"] = serde_json::Value::Bool(
            self.state
                .device
                .features()
                .contains(wgpu::Features::TIMESTAMP_QUERY),
        );
        Ok(result)
    }

    /// Bounded hidden normal-surface diagnostic snapshot; no GPU wait/readback.
    pub fn surface_pacing_snapshot(&self) -> Option<serde_json::Value> {
        self.state.surface_pacing_snapshot()
    }

    /// Render the frame
    pub fn render(&mut self) -> Result<()> {
        self.native_route_last_encoded.set(false);
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
        let overlay_cpu_timer = self.state.surface_pacing_clock();
        let mut egui_output = self.prepare_overlay(true)?;
        self.state
            .surface_pacing_record(crate::surface_pacing::Stage::OverlayCpu, overlay_cpu_timer);
        let chart_text_host_timer = self.state.surface_pacing_clock();
        self.prepare_chart_text(&mut egui_output)?;
        self.state.surface_pacing_record(
            crate::surface_pacing::Stage::ChartTextHost,
            chart_text_host_timer,
        );
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
        let geometry_host_timer = self.state.surface_pacing_clock();
        let geometry_result = self.prepare_geometry_buffers();
        self.state.surface_pacing_record(
            crate::surface_pacing::Stage::GeometryHost,
            geometry_host_timer,
        );
        geometry_result?;
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
        let timestamp_ticket = self.gpu_timestamp_batch.as_mut().and_then(|batch| {
            if self.gpu_profiler.is_enabled()
                || !batch.matches_device_surface(
                    &self.state.device,
                    &self.state.queue,
                    self.state.config.format,
                )
            {
                batch.drop_frame();
                None
            } else {
                batch.reserve(crate::gpu_frame_timestamp::FrameKey {
                    camera: self.vector_emission.gpu_timestamp_target_camera,
                    source_revision: self.vector_emission.triangulation_revision,
                    surface_size: [self.state.config.width, self.state.config.height],
                    view_affine_bits: [
                        self.vector_emission
                            .vector_frame
                            .screen_pan_offset
                            .0
                            .to_bits(),
                        self.vector_emission
                            .vector_frame
                            .screen_pan_offset
                            .1
                            .to_bits(),
                        self.vector_emission
                            .vector_frame
                            .screen_zoom_scale
                            .to_bits(),
                    ],
                })
            }
        });
        // Pass timestamps need only TIMESTAMP_QUERY; encoder timestamps are optional.
        let chart_query = self.gpu_profiler.is_enabled().then(|| {
            self.gpu_profiler.profiler.begin_pass_query(
                "chart_pass",
                &mut encoder,
                &self.state.device,
            )
        });
        let world_query = (self.vector_emission.retained_world_areas.active()
            && self.gpu_profiler.is_enabled())
        .then(|| {
            self.gpu_profiler.profiler.begin_pass_query(
                "retained_world_area_compute",
                &mut encoder,
                &self.state.device,
            )
        });
        self.vector_emission
            .retained_world_areas
            .encode(&mut encoder, world_query.as_ref());
        if let Some(query) = world_query {
            self.gpu_profiler.profiler.end_query(&mut encoder, query);
        }
        self.encode_chart(
            &mut encoder,
            target_view,
            resolve_target,
            chart_query.as_ref(),
            timestamp_ticket.and_then(|ticket| {
                self.gpu_timestamp_batch
                    .as_ref()
                    .map(|batch| batch.writes(ticket))
            }),
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
        let submit_host_timer = self.state.surface_pacing_clock();
        self.state.queue.submit(std::iter::once(encoder.finish()));
        self.state
            .surface_pacing_record(crate::surface_pacing::Stage::SubmitHost, submit_host_timer);
        if let (Some(batch), Some(ticket)) = (&mut self.gpu_timestamp_batch, timestamp_ticket) {
            batch.commit(ticket);
        }

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
        let present_call_timer = self.state.surface_pacing_clock();
        output.present();
        self.state.surface_pacing_record(
            crate::surface_pacing::Stage::PresentCall,
            present_call_timer,
        );
        self.state.surface_pacing_presented();
        drop(flat_present_span);
        if let Some(t) = present_timer {
            self.cpu_profiler.record("present", t.elapsed());
        }

        if let Some(t) = render_timer {
            self.cpu_profiler.record("render_total", t.elapsed());
        }

        self.state.surface_pacing_finish();
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
        if let Some(prepared) = &self.vector_emission.vector_frame.prepared_coverage {
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
                let scales = pass.frame().scale_annotations().map(|annotation| serde_json::json!({
                    "viewing_denominator": annotation.viewing_denominator,
                    "reference_point": annotation.reference_point,
                    "physical_extent": annotation.physical_extent(),
                    "coverages": annotation.coverages.iter().map(|row| serde_json::json!({
                        "dataset_id": row.dataset_id, "coverage_id": row.coverage_id,
                        "minimum_denominator": row.scales.minimum_denominator,
                        "optimum_denominator": row.scales.optimum_denominator,
                        "maximum_denominator": row.scales.maximum_denominator,
                        "selection_band": row.selection_band,
                        "selected_to_fill_gap": row.selected_to_fill_gap,
                        "pattern_eligible": row.selected_to_fill_gap && annotation.viewing_denominator < f64::from(row.scales.maximum_denominator),
                    })).collect::<Vec<_>>(),
                }));
                passes.push(serde_json::json!({"pass":index,"decisions":decisions,"masks":masks,"scale_annotations":scales}));
            }
        }
        std::fs::write(path.join("coverage.json"),serde_json::to_vec_pretty(&serde_json::json!({"bound":self.vector_emission.vector_frame.prepared_coverage.is_some(),"source_datasets":ids,"instruction_count":context.instruction_count(),"passes":passes})).map_err(|e|WgpuError::Render(e.to_string()))?).map_err(|e|WgpuError::Render(e.to_string()))?;
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

        let mut overlay = self.prepare_overlay(include_panels)?;
        self.prepare_chart_text(&mut overlay)?;
        self.prepare_geometry_buffers()?;
        let (target_view, resolve_target) = if let Some(ref mv) = msaa_view {
            (mv, Some(&resolve_view))
        } else {
            (&resolve_view, None)
        };
        // Image exports do not open a profiler frame. Keep their compute
        // behavior identical without leaving an unresolved timestamp query.
        self.vector_emission
            .retained_world_areas
            .encode(&mut encoder, None);
        self.encode_chart(&mut encoder, target_view, resolve_target, None, None);

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
            area_vertices: self
                .vector_emission
                .vector_frame
                .frame_cpu
                .area_vertices
                .len(),
            area_triangles: self
                .vector_emission
                .vector_frame
                .frame_cpu
                .area_indices
                .len()
                / 3,
            line_vertices: self
                .vector_emission
                .vector_frame
                .frame_cpu
                .line_geometry
                .vertex_len(),
            line_triangles: self
                .vector_emission
                .vector_frame
                .frame_cpu
                .line_geometry
                .index_len()
                / 3,
            symbol_instances: self
                .vector_emission
                .vector_frame
                .frame_cpu
                .symbol_instances
                .len(),
            symbol_textures: self.vector_emission.symbol_textures.len(),
            text_labels: self
                .vector_emission
                .vector_frame
                .frame_cpu
                .text_labels
                .len(),
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

fn coverage_scale_sclbr(profile: Option<&ColorProfile>) -> Option<egui::Color32> {
    let colour = profile?.get_srgb("SCLBR")?;
    Some(egui::Color32::from_rgb(colour.r, colour.g, colour.b))
}
#[cfg(test)]
mod coverage_scale_colour_tests {
    use super::*;
    use ferrite_portrayal_catalog::{ColorDefinition, SrgbColor};
    #[test]
    fn active_profile_sclbr_is_used_without_theme_or_named_profile_guessing() {
        for (name, rgb) in [
            ("Day", [12, 34, 56]),
            ("Dusk", [78, 90, 12]),
            ("Night", [34, 56, 78]),
        ] {
            let mut profile = ColorProfile::new(name.into(), name.into());
            profile.colors.insert(
                "SCLBR".into(),
                ColorDefinition {
                    token: "SCLBR".into(),
                    srgb: Some(SrgbColor::new(rgb[0], rgb[1], rgb[2])),
                    cie: None,
                },
            );
            assert_eq!(
                coverage_scale_sclbr(Some(&profile)),
                Some(egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]))
            );
            profile.colors.remove("SCLBR");
            assert_eq!(coverage_scale_sclbr(Some(&profile)), None);
        }
        assert_eq!(coverage_scale_sclbr(None), None);
    }
}

#[cfg(test)]
#[path = "chart_characteristic_gpu_tests.rs"]
mod chart_characteristic_gpu_tests;

#[cfg(test)]
impl VectorFrameState {
    fn empty_fixture() -> Self {
        Self {
            frame_cpu: VectorGeometryCpu::default(),
            geometry_transform: None,
            temporal_visibility_counts: (0, 0),
            temporal_visibility_mask: Vec::new(),
            display_scale: 1,
            coverage_scale_colour: None,
            coverage_scale_colours: Default::default(),
            screen_pan_offset: (0.0, 0.0),
            selection_anchor: None,
            selection_world_geometry: Vec::new(),
            selection_screen_geometry: Vec::new(),
            selection_index: std::cell::OnceCell::new(),
            dependency_status: DependencyRenderStatus::default(),
            screen_zoom_scale: 1.0,
            screen_zoom_scale_y: 1.0,
            screen_zoom_pivot: (0.0, 0.0),
            view_dependent_symbols: false,
            view_clipped_patterns: false,
            scene_bounds: None,
            viewport_world_bounds: None,
            prepared_coverage: None,
            overscale_annotation: Vec::new(),
            coverage_frame: None,
            coverage_failed: false,
            emitting_coverage_source: None,
            device_fixed_sources: FxHashSet::default(),
            static_source_classification: None,
            chart_geometry_viewport: None,
            world_map_chart_boxes: Vec::new(),
            lon_wrap_screen_px: 0.0,
        }
    }
}
#[cfg(test)]
mod vector_frame_state_tests {
    use super::*;
    #[test]
    fn owned_group_move_retains_geometry_camera_permissions_and_selection_together() {
        let mut source = VectorFrameState::empty_fixture();
        source.frame_cpu.displayed_geometry.extend([3, 8]);
        source.frame_cpu.world_map_line_indices.extend([0, 1]);
        source.frame_cpu.world_map_mask_indices.extend([4, 5, 6]);
        source.display_scale = 12999;
        source.screen_pan_offset = (3.25, -5.);
        source.screen_zoom_scale = 2.;
        source.screen_zoom_scale_y = 1.25;
        source.screen_zoom_pivot = (4., 6.);
        source.lon_wrap_screen_px = 1024.;
        source.temporal_visibility_counts = (1, 2);
        source.temporal_visibility_mask = vec![false, true];
        source.device_fixed_sources.insert(8);
        source.dependency_status.permitted = vec![false, true];
        source.dependency_status.executed = vec![false, true];
        source.selection_anchor = Some([12., 13.]);
        source
            .selection_world_geometry
            .push(vec![WorldPoint::new(2., 3.)]);
        let cpu_owner = Arc::clone(&source.frame_cpu.owner);
        let moved = source;
        assert!(Arc::ptr_eq(&cpu_owner, &moved.frame_cpu.owner));
        assert_eq!(moved.frame_cpu.displayed_geometry, [3, 8]);
        assert_eq!(moved.frame_cpu.world_map_line_indices, [0, 1]);
        assert_eq!(moved.frame_cpu.world_map_mask_indices, [4, 5, 6]);
        assert_eq!(moved.display_scale, 12999);
        assert_eq!(moved.screen_pan_offset, (3.25, -5.));
        assert_eq!(
            (moved.screen_zoom_scale, moved.screen_zoom_scale_y),
            (2., 1.25)
        );
        assert_eq!(moved.screen_zoom_pivot, (4., 6.));
        assert_eq!(moved.lon_wrap_screen_px, 1024.);
        assert_eq!(moved.temporal_visibility_counts, (1, 2));
        assert_eq!(moved.temporal_visibility_mask, [false, true]);
        assert!(moved.device_fixed_sources.contains(&8));
        assert_eq!(moved.dependency_status.permitted, [false, true]);
        assert_eq!(moved.dependency_status.executed, [false, true]);
        assert_eq!(moved.selection_anchor, Some([12., 13.]));
        assert_eq!(
            moved.selection_world_geometry[0][0],
            WorldPoint::new(2., 3.)
        );
    }
    #[test]
    fn independent_groups_keep_original_empty_defaults_and_chart_only_trial_prefix() {
        let mut a = VectorFrameState::empty_fixture();
        let b = VectorFrameState::empty_fixture();
        assert!(!Arc::ptr_eq(&a.frame_cpu.owner, &b.frame_cpu.owner));
        assert_eq!(a.display_scale, 1);
        assert_eq!(a.screen_pan_offset, (0., 0.));
        assert_eq!((a.screen_zoom_scale, a.screen_zoom_scale_y), (1., 1.));
        assert_eq!(a.screen_zoom_pivot, (0., 0.));
        assert_eq!(a.lon_wrap_screen_px, 0.);
        assert!(a.prepared_coverage.is_none() && a.coverage_frame.is_none());
        assert!(!a.coverage_failed);
        a.frame_cpu.world_map_line_indices.extend([5, 6]);
        a.frame_cpu.world_map_mask_indices.extend([2, 3, 4]);
        let prefix = a.frame_cpu.prefix();
        assert_eq!(prefix.lengths, [0; 13]);
        a.frame_cpu.area_indices.extend([0, 0, 0]);
        a.frame_cpu.displayed_geometry.push(1);
        a.frame_cpu.truncate_trial(&prefix).unwrap();
        assert_eq!(a.frame_cpu.world_map_line_indices, [5, 6]);
        assert_eq!(a.frame_cpu.world_map_mask_indices, [2, 3, 4]);
        assert_eq!(a.frame_cpu.lengths(), [0; 13]);
        assert!(a.frame_cpu.truncate_trial(&b.frame_cpu.prefix()).is_err());
    }
}

#[cfg(test)]
mod private_lane_transform_key_tests {
    use super::*;
    #[test]
    fn candidate_transform_key_is_independent_of_published_wrap() {
        let mut published = VectorFrameState::empty_fixture();
        let mut candidate = VectorFrameState::empty_fixture();
        published.lon_wrap_screen_px = 1.;
        candidate.lon_wrap_screen_px = 1000.;
        candidate.screen_pan_offset = (-0.0, 3.);
        candidate.screen_zoom_scale = 2.;
        candidate.screen_zoom_scale_y = 4.;
        candidate.screen_zoom_pivot = (5., 6.);
        assert_eq!(
            candidate.continuous_transform_key((800., 600.)),
            [
                800f32.to_bits(),
                600f32.to_bits(),
                (-0.0f32).to_bits(),
                3f32.to_bits(),
                2f32.to_bits(),
                4f32.to_bits(),
                5f32.to_bits(),
                6f32.to_bits(),
                1000f32.to_bits()
            ]
        );
        assert_eq!(
            published.continuous_transform_key((800., 600.))[8],
            1f32.to_bits()
        );
        assert_ne!(
            candidate.continuous_transform_key((800., 600.)),
            published.continuous_transform_key((800., 600.))
        );
        assert_ne!(
            candidate.continuous_transform_key((801., 600.)),
            candidate.continuous_transform_key((800., 600.))
        );
    }
}

struct VectorEmissionOwned {
    scene_draw_plan: vector_scene_draw::scene_draw_plan::Cache,
    primary_line_quad_enabled: bool,
    accepted_screen_line_packet: crate::accepted_screen_line_packet::Capture,
    area_projection_shadow: crate::retained_world_area::ExactAreaShadowCache<CachedTriangulation>,
    area_triangulation_reuse_enabled: bool,
    cached_symbol_buffers: EmittedSymbolBuffers,
    compact_owner_admission_diagnostics: bool,
    compact_owner_admission_enabled: bool,
    current_frame_admission_reuse_enabled: bool,
    compact_owner_work: crate::compact_owner_admission::Work,
    coverage_pipelines: Option<Arc<crate::coverage_pipeline::CoveragePipelines>>,
    coverage_trial_reuse: bool,
    coverage_trial_work: crate::coverage_trial::Work,
    emitter_wave_work: Option<crate::emitter_wave_diagnostics::Collector>,
    empty_symbol_ref_count: u32,
    flat_diagnostic: Option<FlatDiagnosticCell>,
    flat_gpu_coverage_host_ns: u64,
    flat_stage_timing_enabled: bool,
    frame_local_coverage_clip_reuse: bool,
    gpu_buffers_dirty: bool,
    gpu_timestamp_requested: bool,
    gpu_timestamp_target_camera: Option<[u64; 16]>,
    line_preparation_work: Option<crate::line_preparation_diagnostics::Collector>,
    line_suppression: ferrite_render::LineSuppressionCache,
    missing_symbol_ids: FxHashSet<(u64, SymbolId)>,
    moving_line_northing: crate::moving_line_northing::Cache,
    source_batch_parallel: crate::source_batch_parallel::Cache,
    native_route_target_camera: Option<[u64; 16]>,
    owner_draw_borrow_enabled: bool,
    owner_group_plan_diagnostics: bool,
    owner_group_plan_enabled: bool,
    owner_group_work: crate::owner_group_plan::Work,
    pattern_textures: HashMap<String, PatternTexture>,
    referenced_chart_owner: Option<crate::referenced_chart_owner::ReferencedChartOwner>,
    retained_area_triangulations: AreaTriangulationRetention,
    retained_world_areas: crate::retained_world_area::RetainedWorldAreas,
    source_line_projection_arena: crate::source_line_projection_arena::Cache,
    spatial_hierarchy_enabled: bool,
    dense_area_candidates_baseline: bool,
    static_line_bounds: crate::static_line_bounds::Cache,
    static_source_classification_enabled: bool,
    suppression_tail: Option<crate::suppression_tail::Collector>,
    symbol_class_cache: FxHashMap<SymbolId, u8>,
    symbol_textures: HashMap<(u64, ferrite_render::SymbolId), SymbolTexture>,
    triangulation_cache:
        HashMap<(usize, usize, ferrite_render::FlatProjection), TriangulationStorage>,
    triangulation_failures: FxHashSet<(usize, usize, ferrite_render::FlatProjection)>,
    triangulation_revision: Option<u64>,
    vector_frame: VectorFrameState,
}

/// A metric source is never a shallow clone of the live UI Context.
enum EmissionFonts<'a> {
    Live(&'a mut EguiIntegration),
    ReadOnly(&'a egui::Context),
    Private(&'a egui::Context),
}
impl EmissionFonts<'_> {
    fn context(&self) -> &egui::Context {
        match self {
            Self::Live(ui) => &ui.ctx,
            Self::ReadOnly(ctx) | Self::Private(ctx) => ctx,
        }
    }
    fn is_private(&self) -> bool {
        matches!(self, Self::Private(_))
    }
    fn ensure_font_metrics(&mut self, window: &Window) {
        match self {
            Self::Live(ui) => ui.ensure_font_metrics(window),
            // Private metric source is independently initialized before emission.
            // All actual chart labels are laid out by its independently prepared owner.
            Self::Private(_) => {}
            Self::ReadOnly(_) => {
                unreachable!("Read-only emitter helper cannot request font metrics")
            }
        }
    }
}
struct EmissionServices<'a> {
    state: &'a GpuState,
    pipelines: &'a RenderPipelines,
    fonts: EmissionFonts<'a>,
    cpu_profiler: Option<&'a mut CpuProfiler>,
    overscale_program_reuse: &'a crate::overscale_annotation::ProgramReuse,
    symbol_scale: f32,
    show_soundings: bool,
    animation_mode: bool,
    show_shallow_pattern: bool,
    world_map_coastlines: &'a ferrite_render::BackgroundCoastlines,
    world_map_detailed: &'a ferrite_render::BackgroundCoastlines,
    background_color: Color,
}
impl<'a> EmissionServices<'a> {
    #[expect(
        clippy::too_many_arguments,
        reason = "Explicit immutable device and independently owned chart-font inputs retain existing emitter behavior"
    )]
    fn live(
        state: &'a GpuState,
        pipelines: &'a RenderPipelines,
        egui: &'a mut EguiIntegration,
        cpu: &'a mut CpuProfiler,
        programs: &'a crate::overscale_annotation::ProgramReuse,
        symbol_scale: f32,
        show_soundings: bool,
        animation_mode: bool,
        shallow: bool,
        world_map_coastlines: &'a ferrite_render::BackgroundCoastlines,
        world_map_detailed: &'a ferrite_render::BackgroundCoastlines,
        background_color: Color,
    ) -> Self {
        Self {
            state,
            pipelines,
            fonts: EmissionFonts::Live(egui),
            cpu_profiler: Some(cpu),
            overscale_program_reuse: programs,
            symbol_scale,
            show_soundings,
            animation_mode,
            show_shallow_pattern: shallow,
            world_map_coastlines,
            world_map_detailed,
            background_color,
        }
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Read-only helper keeps the same device and display inputs without mutable UI/font access"
    )]
    fn read_only(
        state: &'a GpuState,
        pipelines: &'a RenderPipelines,
        ctx: &'a egui::Context,
        programs: &'a crate::overscale_annotation::ProgramReuse,
        symbol_scale: f32,
        show_soundings: bool,
        animation_mode: bool,
        shallow: bool,
        world_map_coastlines: &'a ferrite_render::BackgroundCoastlines,
        world_map_detailed: &'a ferrite_render::BackgroundCoastlines,
        background_color: Color,
    ) -> Self {
        Self {
            state,
            pipelines,
            fonts: EmissionFonts::ReadOnly(ctx),
            cpu_profiler: None,
            overscale_program_reuse: programs,
            symbol_scale,
            show_soundings,
            animation_mode,
            show_shallow_pattern: shallow,
            world_map_coastlines,
            world_map_detailed,
            background_color,
        }
    }
    fn record(&mut self, name: &'static str, elapsed: std::time::Duration) {
        if let Some(profiler) = self.cpu_profiler.as_mut() {
            profiler.record(name, elapsed);
        }
    }
}

impl VectorEmissionOwned {
    /// Create, on the build thread, the large buffers the UI thread would
    /// upload for this scene. The legacy line materialization it performs is
    /// the same idempotent step `prepare_geometry_buffers` runs.
    fn prebuild_scene_buffers(&mut self, gpu: &GpuState) {
        use wgpu::BufferUsages as U;
        let cpu = &mut self.vector_frame.frame_cpu;
        if !self.retained_world_areas.active() {
            gpu.prebuild_buffer(
                "area_vertices",
                U::VERTEX,
                bytemuck::cast_slice(&cpu.area_vertices),
            );
        }
        gpu.prebuild_buffer(
            "area_indices",
            U::INDEX,
            bytemuck::cast_slice(&cpu.area_indices),
        );
        if cpu.line_geometry.materialize().is_ok() {
            if let Some((vertices, indices)) = cpu.line_geometry.legacy() {
                gpu.prebuild_buffer("line_vertices", U::VERTEX, bytemuck::cast_slice(vertices));
                gpu.prebuild_buffer("line_indices", U::INDEX, bytemuck::cast_slice(indices));
            }
        }
        gpu.prebuild_buffer(
            "pattern_vertices",
            U::VERTEX,
            bytemuck::cast_slice(&cpu.pattern_vertices),
        );
        self.vector_frame.compute_scene_bounds();
        let cpu = &mut self.vector_frame.frame_cpu;
        gpu.prebuild_buffer(
            "pattern_indices",
            U::INDEX,
            bytemuck::cast_slice(&cpu.pattern_indices),
        );
    }

    /// Reset per-scene output before an emission; caches are retained.
    fn begin_frame(&mut self, exact_line_quad: bool) {
        self.accepted_screen_line_packet.retire_frame();
        self.coverage_trial_work = Default::default();
        self.retained_world_areas.begin_frame();
        // Mark GPU buffers as needing rebuild
        self.gpu_buffers_dirty = true;
        // Invalidate cached symbol GPU buffers (geometry changed)
        self.cached_symbol_buffers.clear();

        // Preserve previous frame counts for pre-allocation (avoids realloc during build)
        let prev_area_v = self.vector_frame.frame_cpu.area_vertices.len();
        let prev_area_i = self.vector_frame.frame_cpu.area_indices.len();

        self.vector_frame.frame_cpu.area_vertices.clear();
        self.vector_frame.frame_cpu.area_indices.clear();
        self.vector_frame
            .frame_cpu
            .line_geometry
            .begin_frame(self.primary_line_quad_enabled && exact_line_quad);
        self.vector_frame.frame_cpu.symbol_instances.clear();
        self.vector_frame.frame_cpu.displayed_geometry.clear();
        self.vector_frame.selection_index.take();
        self.vector_frame.dependency_status = DependencyRenderStatus::default();

        // Reserve capacity based on previous frame (amortized zero reallocs in steady state)
        if prev_area_v > self.vector_frame.frame_cpu.area_vertices.capacity() / 2 {
            self.vector_frame
                .frame_cpu
                .area_vertices
                .reserve(prev_area_v);
        }
        if prev_area_i > self.vector_frame.frame_cpu.area_indices.capacity() / 2 {
            self.vector_frame
                .frame_cpu
                .area_indices
                .reserve(prev_area_i);
        }
        // Clear symbol batches for new frame
        // Clear priority ranges for S-101 compliant rendering
        self.vector_frame.device_fixed_sources.clear();
        self.vector_frame.static_source_classification = None;
        self.vector_frame.prepared_coverage = None;
        self.vector_frame.overscale_annotation.clear();
        self.vector_frame.coverage_frame = None;
        self.vector_frame.coverage_failed = false;
        self.vector_frame.coverage_scale_colour = None;
        self.vector_frame.coverage_scale_colours.clear();
        self.vector_frame.emitting_coverage_source = None;
        self.vector_frame.frame_cpu.area_priority_ranges.clear();
        self.vector_frame.frame_cpu.line_priority_ranges.clear();
        self.vector_frame.frame_cpu.symbol_priority_ranges.clear();
        self.vector_frame.frame_cpu.pattern_vertices.clear();
        self.vector_frame.frame_cpu.pattern_indices.clear();
        self.vector_frame.frame_cpu.pattern_ranges.clear();
        if let Some(records) = self.vector_frame.frame_cpu.pattern_emission_audit.as_mut() {
            records.clear();
        }
        self.vector_frame.frame_cpu.pattern_emission_audit_dropped = 0;
        self.vector_frame.view_clipped_patterns = false;
        self.vector_frame.scene_bounds = None;
        self.vector_frame.frame_cpu.text_labels.clear();
        // World map separate buffers
        self.vector_frame.frame_cpu.world_map_line_vertices.clear();
        self.vector_frame.frame_cpu.world_map_line_indices.clear();
        self.vector_frame.frame_cpu.world_map_mask_vertices.clear();
        self.vector_frame.frame_cpu.world_map_mask_indices.clear();

        // General PC PointInstructions retain every eligible command. S-100
        // CoverageFill NumericAnnotation collision rules are a separate primitive;
        // they do not authorize thinning S-101 SOUNDG03 point symbols.
    }

    fn update_viewport_bounds(
        &mut self,
        _services: &mut EmissionServices<'_>,
        scaler: &ferrite_render::Scaler,
    ) {
        if self.gpu_timestamp_requested {
            self.gpu_timestamp_target_camera = scaler.flat_encoded_identity();
        }
        self.native_route_target_camera = scaler.flat_encoded_identity();
        let (vw, vh) = (scaler.viewport.width, scaler.viewport.height);
        let top_left = scaler.screen_to_world(ScreenPoint { x: 0.0, y: 0.0 });
        let bottom_right = scaler.screen_to_world(ScreenPoint { x: vw, y: vh });
        self.vector_frame.viewport_world_bounds = Some((
            top_left.x.min(bottom_right.x),
            top_left.y.min(bottom_right.y),
            top_left.x.max(bottom_right.x),
            top_left.y.max(bottom_right.y),
        ));
    }
    fn is_point_visible(&self, _services: &mut EmissionServices<'_>, x: f64, y: f64) -> bool {
        longitude_bounds_visible(
            (x, y, x, y),
            self.vector_frame.viewport_world_bounds,
            self.vector_frame.lon_wrap_screen_px > 0.0,
        )
    }
    fn is_aabb_visible(
        &self,
        _services: &EmissionServices<'_>,
        ax: f64,
        ay: f64,
        bx: f64,
        by: f64,
    ) -> bool {
        longitude_bounds_visible(
            (ax, ay, bx, by),
            self.vector_frame.viewport_world_bounds,
            self.vector_frame.lon_wrap_screen_px > 0.0,
        )
    }
    fn bind_triangulation_context(
        &mut self,
        _services: &mut EmissionServices<'_>,
        context: &RenderContext,
    ) {
        self.retained_world_areas
            .bind_epoch(context.geometry_revision());
        let revision = context.geometry_revision();
        self.area_projection_shadow.bind_epoch(revision);
        self.moving_line_northing.bind_epoch(revision);
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
    fn symbol_quad(
        &self,
        services: &mut EmissionServices<'_>,
        instance: &SymbolInstance,
    ) -> Option<[TextureVertex; 4]> {
        let tex = self
            .symbol_textures
            .get(&(instance.resource_owner, instance.symbol_id))?;
        if !tex.has_coverage {
            return None;
        }
        let (vp_w, vp_h) = services.state.viewport_size();
        let sym_min_x = -200.;
        let sym_min_y = -200.;
        let sym_max_x = vp_w + 200.;
        let sym_max_y = vp_h + 200.;
        let display_scale = instance.scale / tex.render_scale
            * services.symbol_scale
            * services.state.scale_factor() as f32;
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
        let offsets = [
            0.,
            -self.vector_frame.lon_wrap_screen_px,
            self.vector_frame.lon_wrap_screen_px,
        ];
        let copies = if self.vector_frame.lon_wrap_screen_px > 0.
            && !instance
                .source
                .is_some_and(|s| self.is_device_fixed_source(services, s))
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
    fn coverage_clip_transform(
        &self,
        _services: &mut EmissionServices<'_>,
    ) -> Result<crate::coverage_clip::ClipTransform> {
        let [sx, sy] = [
            self.vector_frame.screen_zoom_scale,
            self.vector_frame.screen_zoom_scale_y,
        ];
        let (px, py) = self.vector_frame.screen_zoom_pivot;
        crate::coverage_clip::ClipTransform::new(
            [1. / sx, 1. / sy],
            [
                px - px / sx - self.vector_frame.screen_pan_offset.0,
                py - py / sy - self.vector_frame.screen_pan_offset.1,
            ],
        )
    }
    fn update_coverage_transform(&mut self, services: &mut EmissionServices<'_>) {
        let transform = self.coverage_clip_transform(services);
        if let Some(frame) = &mut self.vector_frame.coverage_frame {
            match transform {
                Ok(transform) => frame.set_transform(&services.state.queue, transform),
                Err(error) => {
                    self.vector_frame.coverage_failed = true;
                    tracing::error!("Coverage affine rejected: {error}");
                }
            }
        }
    }
    fn is_device_fixed_source(&self, _services: &mut EmissionServices<'_>, ordinal: usize) -> bool {
        self.vector_frame
            .static_source_classification
            .as_ref()
            .map_or_else(
                || self.vector_frame.device_fixed_sources.contains(&ordinal),
                |classification| classification.is_device_fixed(ordinal),
            )
    }
    fn flat_span(
        &self,
        _services: &mut EmissionServices<'_>,
        stage: ferrite_render::flat_reuse_diagnostics::FlatFrameStage,
    ) -> Option<FlatDiagnosticSpan> {
        if !self.flat_stage_timing_enabled {
            return None;
        }
        self.flat_diagnostic
            .as_ref()
            .map(|cell| FlatDiagnosticSpan {
                cell: cell.clone(),
                stage,
                start: std::time::Instant::now(),
            })
    }
    fn record_flat_stage(
        &self,
        _services: &mut EmissionServices<'_>,
        stage: ferrite_render::flat_reuse_diagnostics::FlatFrameStage,
        elapsed: std::time::Duration,
    ) {
        if let Some(cell) = &self.flat_diagnostic {
            cell.borrow_mut()
                .record_span(stage, elapsed.as_nanos().min(u64::MAX as u128) as u64);
        }
    }
    fn viewing_scale(&self, _services: &mut EmissionServices<'_>) -> u32 {
        self.vector_frame.display_scale
    }
    fn add_world_map_lines(
        &mut self,
        services: &mut EmissionServices<'_>,
        scaler: &ferrite_render::Scaler,
    ) {
        if services.world_map_coastlines.is_empty() && services.world_map_detailed.is_empty() {
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
            && !services.world_map_detailed.is_empty();
        let coastlines = if detailed {
            &services.world_map_detailed
        } else {
            &services.world_map_coastlines
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

                    if let Some((cx0, cy0, cx1, cy1)) = WgpuRenderer::clip_line_segment(
                        prev.x, prev.y, curr.x, curr.y, clip_x_min, clip_y_min, clip_x_max,
                        clip_y_max,
                    ) {
                        let dx = cx1 - cx0;
                        let dy = cy1 - cy0;
                        let len = (dx * dx + dy * dy).sqrt();

                        if len >= 0.5 {
                            let nx = -dy / len * width * 0.5;
                            let ny = dx / len * width * 0.5;

                            let base_index =
                                self.vector_frame.frame_cpu.world_map_line_vertices.len() as u32;

                            self.vector_frame
                                .frame_cpu
                                .world_map_line_vertices
                                .push(LineVertex::new(cx0, cy0, -nx, -ny, color));
                            self.vector_frame
                                .frame_cpu
                                .world_map_line_vertices
                                .push(LineVertex::new(cx0, cy0, nx, ny, color));
                            self.vector_frame
                                .frame_cpu
                                .world_map_line_vertices
                                .push(LineVertex::new(cx1, cy1, nx, ny, color));
                            self.vector_frame
                                .frame_cpu
                                .world_map_line_vertices
                                .push(LineVertex::new(cx1, cy1, -nx, -ny, color));

                            self.vector_frame
                                .frame_cpu
                                .world_map_line_indices
                                .push(base_index);
                            self.vector_frame
                                .frame_cpu
                                .world_map_line_indices
                                .push(base_index + 1);
                            self.vector_frame
                                .frame_cpu
                                .world_map_line_indices
                                .push(base_index + 2);
                            self.vector_frame
                                .frame_cpu
                                .world_map_line_indices
                                .push(base_index);
                            self.vector_frame
                                .frame_cpu
                                .world_map_line_indices
                                .push(base_index + 2);
                            self.vector_frame
                                .frame_cpu
                                .world_map_line_indices
                                .push(base_index + 3);
                        }
                    }

                    prev = curr;
                }
            }

            // Add opaque background rectangles over chart bboxes at this lon offset.
            // These mask world map coastlines under loaded chart areas.
            let bg = services.background_color.to_array();
            for &(min_x, min_y, max_x, max_y) in &self.vector_frame.world_map_chart_boxes {
                let shifted_min_x = min_x + lon_offset;
                let shifted_max_x = max_x + lon_offset;

                // Frustum cull
                if !self.is_aabb_visible(services, shifted_min_x, min_y, shifted_max_x, max_y) {
                    continue;
                }

                let tl = scaler.world_to_screen(WorldPoint::new(shifted_min_x, max_y));
                let br = scaler.world_to_screen(WorldPoint::new(shifted_max_x, min_y));

                if !tl.x.is_finite() || !tl.y.is_finite() || !br.x.is_finite() || !br.y.is_finite()
                {
                    continue;
                }

                let base = self.vector_frame.frame_cpu.world_map_mask_vertices.len() as u32;
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_vertices
                    .push(Vertex2D::new(tl.x, tl.y, bg));
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_vertices
                    .push(Vertex2D::new(br.x, tl.y, bg));
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_vertices
                    .push(Vertex2D::new(br.x, br.y, bg));
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_vertices
                    .push(Vertex2D::new(tl.x, br.y, bg));

                self.vector_frame
                    .frame_cpu
                    .world_map_mask_indices
                    .push(base);
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_indices
                    .push(base + 1);
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_indices
                    .push(base + 2);
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_indices
                    .push(base);
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_indices
                    .push(base + 2);
                self.vector_frame
                    .frame_cpu
                    .world_map_mask_indices
                    .push(base + 3);
            }
        }
    }
    fn add_instructions_with_resources_impl(
        &mut self,
        services: &mut EmissionServices<'_>,
        context: &mut RenderContext,
        mut symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
        mut resource_owners: Option<&mut crate::CellPortrayalResources>,
    ) -> Result<()> {
        let mut coverage_trial = crate::coverage_trial::Invocation::default();
        if self.coverage_trial_reuse {
            self.coverage_trial_work.invocations =
                self.coverage_trial_work.invocations.saturating_add(1);
        }
        self.vector_frame.coverage_scale_colour = coverage_scale_sclbr(color_profile);
        self.vector_frame.coverage_scale_colours.clear();
        if let Some(resources) = resource_owners.as_deref() {
            for (cell, profile) in resources.cell_profiles() {
                if let Some(colour) = coverage_scale_sclbr(Some(profile)) {
                    self.vector_frame
                        .coverage_scale_colours
                        .insert(cell, colour);
                }
            }
        }
        context.get_sorted_instructions();
        let flat_dependency_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::DependencyResolution,
        );
        let graph = context.dependency_graph();
        if !graph.has_parents() {
            drop(flat_dependency_span);
            self.vector_frame.dependency_status = DependencyRenderStatus {
                converged: true,
                ..Default::default()
            };
            self.emit_instructions_with_symbols(
                services,
                context,
                symbol_cache,
                color_profile,
                visible_viewing_groups,
                true,
                None,
                None,
                resource_owners.as_deref_mut(),
                &mut coverage_trial,
            )?;
            return Ok(());
        }

        // Retain a frame's pre-existing overlay prefix. Cached textures and
        // triangulation remain reusable; trial geometry is never submitted.
        let prefix = self.vector_frame.frame_cpu.prefix();
        let seed = graph
            .resolve(&vec![true; graph.len()])
            .expect("matching dependency graph");
        let mut permission = seed.executed;
        let mut executed = vec![false; graph.len()];
        let mut previous: Option<Vec<bool>> = None;
        self.vector_frame.dependency_status = DependencyRenderStatus {
            missing_parent_count: seed.missing_parent_count,
            ..Default::default()
        };
        // Suppression and collision can feed back into parent execution. Bound
        // trial work and diagnose inconsistent lists rather than hang the UI.
        for pass in 0..65 {
            self.vector_frame.frame_cpu.truncate_trial(&prefix)?;
            self.cached_symbol_buffers.clear();
            self.gpu_buffers_dirty = true;
            executed.fill(false);
            self.emit_instructions_with_symbols(
                services,
                context,
                symbol_cache.as_deref_mut(),
                color_profile,
                visible_viewing_groups,
                false,
                Some(&permission),
                Some(&mut executed),
                resource_owners.as_deref_mut(),
                &mut coverage_trial,
            )?;
            self.vector_frame.dependency_status.iterations = pass + 1;
            if self.coverage_trial_reuse && self.vector_frame.coverage_failed {
                break;
            }

            if self.vector_frame.dependency_status.nonconvergent_diagnostic {
                break;
            }
            let grounded = graph.resolve(&executed).expect("matching execution mask");
            let next = graph
                .permitted_by_executed(&grounded.executed)
                .expect("matching execution mask");
            if next == permission {
                self.vector_frame.dependency_status.converged = true;
                break;
            }
            if previous.as_ref() == Some(&next) || pass == 63 {
                tracing::error!(
                    "S-100 Parent execution did not converge; dependent commands withheld, roots retained"
                );
                self.vector_frame.dependency_status.nonconvergent_diagnostic = true;
                permission = context
                    .raw_instructions()
                    .iter()
                    .map(|i| i.dependency().is_none_or(|d| d.parent_id.is_none()))
                    .collect();
            } else {
                previous = Some(std::mem::replace(&mut permission, next));
            }
        }
        self.vector_frame.dependency_status.permitted = permission;
        self.vector_frame.dependency_status.executed = executed;
        Ok(())
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Mechanical shared emitter preserves independent authority, coverage and ownership inputs"
    )]
    fn emit_instructions_with_symbols(
        &mut self,
        services: &mut EmissionServices<'_>,
        context: &mut RenderContext,
        mut symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
        no_parent: bool,
        dependency_permission: Option<&[bool]>,
        mut execution: Option<&mut [bool]>,
        mut resource_owners: Option<&mut crate::CellPortrayalResources>,
        coverage_trial: &mut crate::coverage_trial::Invocation,
    ) -> Result<()> {
        let mut emitter_wave = self.emitter_wave_work.as_ref().map(|collector| {
            crate::emitter_wave_diagnostics::Call::new(
                collector,
                [
                    self.vector_frame.frame_cpu.area_vertices.len(),
                    self.vector_frame.frame_cpu.line_geometry.vertex_len(),
                    self.vector_frame.frame_cpu.line_geometry.index_len(),
                    self.vector_frame.frame_cpu.symbol_instances.len(),
                    self.vector_frame.frame_cpu.text_labels.len(),
                ],
            )
        });
        // Vector PC group handles do not authorize raster visibility.
        if self.gpu_timestamp_requested {
            self.gpu_timestamp_target_camera = context.scaler.flat_encoded_identity();
        }
        self.vector_frame.chart_geometry_viewport = Some(context.scaler.viewport);
        self.bind_triangulation_context(services, context);
        let profiling = crate::profiler::is_profiling_enabled();
        let total_timer = if profiling {
            Some(ScopeTimer::new("add_instructions_total"))
        } else {
            None
        };

        // Set animation mode and sort instructions, then extract what we need
        context.set_animation_mode(services.animation_mode);
        // get_sorted_instructions() sorts in-place on first call, then returns &slice
        // Clone scaler (cheap: a few f64 fields) to avoid borrow conflict with &mut self methods
        context
            .scaler
            .set_pixel_ratio(services.state.scale_factor());
        let scaler = context.scaler.clone();
        self.vector_frame.geometry_transform = Some(WgpuRenderer::scaler_transform(&scaler));
        self.vector_frame.display_scale =
            scaler.display_scale.round().clamp(1.0, u32::MAX as f64) as u32;
        context.get_sorted_instructions();
        self.accepted_screen_line_packet.begin(
            context,
            [
                self.vector_frame.frame_cpu.line_geometry.vertex_len(),
                self.vector_frame.frame_cpu.line_geometry.index_len(),
            ],
        );
        let cpu_temporal_start = profiling.then(std::time::Instant::now);
        let flat_visibility_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Visibility,
        );
        let (temporal_visible, hidden, diagnostics) = match context.portrayal_visibility() {
            Ok(visibility) => visibility,
            Err(error) => {
                self.vector_frame.coverage_failed = true;
                tracing::error!("Coverage execution visibility rejected: {error}");
                return Err(WgpuError::Render(format!(
                    "Coverage execution visibility rejected: {error}"
                )));
            }
        };
        drop(flat_visibility_span);
        if let Some(start) = cpu_temporal_start {
            services.record("emit_temporal_visibility", start.elapsed());
        }
        if let Some(cell) = &self.flat_diagnostic {
            let mut row = cell.borrow_mut();
            row.work.source_commands = row
                .work
                .source_commands
                .saturating_add(context.raw_instructions().len() as u64);
        }
        if diagnostics > 0 && self.vector_frame.temporal_visibility_counts.1 != diagnostics {
            tracing::warn!(
                "Temporal selector: {diagnostics} primitives preserved due to unsupported or invalid time conditions"
            );
        }
        self.vector_frame.temporal_visibility_counts = (hidden, diagnostics);
        let cpu_classification_start = profiling.then(std::time::Instant::now);
        let flat_classification_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::SourceClassification,
        );
        if self.static_source_classification_enabled {
            self.vector_frame.static_source_classification =
                Some(context.static_source_classification());
        } else {
            self.vector_frame.device_fixed_sources = context
                .raw_instructions()
                .iter()
                .enumerate()
                .filter_map(|(i, command)| {
                    command.portrayal_origin().is_device_fixed().then_some(i)
                })
                .collect();
        }
        drop(flat_classification_span);
        if let Some(start) = cpu_classification_start {
            services.record("emit_classification", start.elapsed());
        }
        let cpu_coverage_start = profiling.then(std::time::Instant::now);
        let flat_gpu_coverage_start = self
            .flat_diagnostic
            .as_ref()
            .map(|_| std::time::Instant::now());
        self.vector_frame.prepared_coverage = match context.prepared_coverage_binding() {
            Ok(binding) => binding,
            Err(error) => {
                self.vector_frame.coverage_failed = true;
                tracing::error!("Coverage binding rejected: {error}");
                return Err(WgpuError::Render(format!(
                    "Coverage binding rejected: {error}"
                )));
            }
        };
        self.vector_frame.emitting_coverage_source = None;
        // GPU resource readiness is independent of parent permission. That
        // permission, temporal visibility and cell groups are evaluated freshly
        // below on every trial; no execution decisions are cached.
        let key = self.coverage_trial_reuse.then(|| {
            crate::coverage_trial::Key::new(
                context,
                self.vector_frame.prepared_coverage.as_ref(),
                [services.state.size.width, services.state.size.height],
                services.state.scale_factor(),
                if self.vector_frame.lon_wrap_screen_px > 0. {
                    3
                } else {
                    1
                },
                [
                    color_profile.map_or(0, |p| p as *const ColorProfile as usize),
                    symbol_cache
                        .as_deref()
                        .map_or(0, |p| p as *const SymbolCache as usize),
                ],
                symbol_cache
                    .as_deref()
                    .map_or(0, SymbolCache::resource_revision),
                services.state.format(),
                crate::state::MSAA_SAMPLE_COUNT,
            )
        });
        let readiness = crate::coverage_trial::Readiness::new(
            self.vector_frame.coverage_frame.as_ref(),
            &self.vector_frame.overscale_annotation,
        );
        let reused = key.as_ref().is_some_and(|key| {
            !self.vector_frame.coverage_failed
                && coverage_trial.matches(
                    key,
                    &temporal_visible,
                    resource_owners.as_deref(),
                    readiness,
                )
        });
        if self.coverage_trial_reuse {
            self.coverage_trial_work.trials = self.coverage_trial_work.trials.saturating_add(1);
        }
        if reused {
            self.coverage_trial_work.reuse_hits =
                self.coverage_trial_work.reuse_hits.saturating_add(1);
        } else {
            coverage_trial.invalidate();
            if self.coverage_trial_reuse {
                self.coverage_trial_work.preparations =
                    self.coverage_trial_work.preparations.saturating_add(1);
            }
            self.vector_frame.coverage_frame = None;
            self.vector_frame.coverage_failed = false;
            self.vector_frame.overscale_annotation.clear();
            let resources = crate::prepared_vector_coverage::prepare_coverage_resources(
                services.state,
                services.pipelines,
                context,
                self.vector_frame.prepared_coverage.as_ref(),
                self.coverage_pipelines.as_ref().map(Arc::clone),
                symbol_cache.as_deref_mut(),
                color_profile,
                resource_owners.as_deref_mut(),
                self.vector_frame.lon_wrap_screen_px,
                self.frame_local_coverage_clip_reuse,
                Some(services.overscale_program_reuse),
                self.coverage_trial_reuse
                    .then_some(&mut self.coverage_trial_work),
            );
            match resources {
                Ok(resources) => {
                    self.vector_frame.coverage_frame = resources.frame;
                    self.coverage_pipelines = resources.pipelines;
                    self.vector_frame.overscale_annotation = resources.annotations;
                }
                Err(error) => {
                    self.vector_frame.coverage_failed = true;
                    tracing::error!("Vector coverage resource preparation rejected: {error}");
                    return Err(WgpuError::Render(format!(
                        "Vector coverage resource preparation rejected: {error}"
                    )));
                }
            }

            if let Some(key) = key {
                let readiness = crate::coverage_trial::Readiness::new(
                    self.vector_frame.coverage_frame.as_ref(),
                    &self.vector_frame.overscale_annotation,
                );
                self.coverage_trial_work.annotations_ready = self
                    .coverage_trial_work
                    .annotations_ready
                    .saturating_add(self.vector_frame.overscale_annotation.len() as u64);
                coverage_trial.complete(
                    key,
                    self.vector_frame.prepared_coverage.clone(),
                    &temporal_visible,
                    resource_owners.as_deref(),
                    readiness,
                );
            }
        }
        self.update_coverage_transform(services);
        if let Some(start) = flat_gpu_coverage_start {
            let elapsed = start.elapsed();
            self.flat_gpu_coverage_host_ns = self
                .flat_gpu_coverage_host_ns
                .saturating_add(elapsed.as_nanos().min(u64::MAX as u128) as u64);
            self.record_flat_stage(
                services,
                ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Coverage,
                elapsed,
            );
        }
        if let Some(start) = cpu_coverage_start {
            services.record("emit_coverage_binding_resources", start.elapsed());
        }
        let instructions = context.raw_instructions();
        // One owned arena view per emitter call; every authority/view gate still runs.
        let cpu_projection_start = profiling.then(std::time::Instant::now);
        let flat_projection_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::SourceProjectionPreparation,
        );
        let source_projection_frame = self.source_line_projection_arena.prepare(context, &scaler);
        // Bind once while the context remains immutably borrowed throughout this emitter.
        let source_bounds_frame = source_projection_frame
            .as_ref()
            .and_then(|frame| frame.bind_source_bounds(context));
        self.static_line_bounds
            .bind(context.static_line_relation_epoch(), instructions.len());
        drop(flat_projection_span);
        if let Some(start) = cpu_projection_start {
            services.record("emit_projection", start.elapsed());
        }
        let cpu_execution_visibility_start = profiling.then(std::time::Instant::now);
        let flat_execution_visibility_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::ExecutionVisibility,
        );
        let visibility = dependency_permission.map(|permission| {
            temporal_visible
                .iter()
                .zip(permission)
                .map(|(date, parent)| *date && *parent)
                .collect::<Vec<_>>()
        });
        let execution_visibility = visibility.as_deref().unwrap_or(&temporal_visible);
        drop(flat_execution_visibility_span);
        if let Some(start) = cpu_execution_visibility_start {
            services.record("emit_execution_visibility", start.elapsed());
        }
        let viewing_scale = self.viewing_scale(services);
        let cpu_compact_start = profiling.then(std::time::Instant::now);
        let flat_compact_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::CompactAdmission,
        );
        let compact_frame = if self.compact_owner_admission_enabled {
            if let Some(resources) = resource_owners.as_deref() {
                match resources.prepare_compact_admission(
                    context,
                    execution_visibility,
                    viewing_scale,
                    services.show_soundings.then_some(33010),
                    self.compact_owner_admission_diagnostics,
                ) {
                    Ok((frame, work)) => {
                        if self.compact_owner_admission_diagnostics {
                            self.compact_owner_work.add(work);
                        }
                        frame
                    }
                    Err(error) => {
                        self.vector_frame.coverage_failed = true;
                        tracing::error!("Mixed-PC visibility rejected: {error}");
                        return Err(WgpuError::Render(format!(
                            "Mixed-PC visibility rejected: {error}"
                        )));
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        drop(flat_compact_span);
        if let Some(start) = cpu_compact_start {
            services.record("emit_compact", start.elapsed());
        }
        let cpu_owner_start = profiling.then(std::time::Instant::now);
        let flat_owner_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::OwnerAdmission,
        );
        let owner_visibility = if compact_frame.is_some() {
            None
        } else if let Some(resources) = resource_owners.as_deref() {
            let owner_start = self
                .owner_group_plan_diagnostics
                .then(std::time::Instant::now);
            let compile_start = self
                .owner_group_plan_diagnostics
                .then(std::time::Instant::now);
            let (group_plan, hit) = if self.owner_group_plan_enabled {
                resources.prepare_owner_group_plan(context)
            } else {
                (None, false)
            };
            if let Some(start) = compile_start {
                let prepare_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                self.owner_group_work.plan_prepare_host_ns = self
                    .owner_group_work
                    .plan_prepare_host_ns
                    .saturating_add(prepare_ns);
                if self.owner_group_plan_enabled {
                    if group_plan.is_none() {
                        self.owner_group_work.plan_decline_host_ns = self
                            .owner_group_work
                            .plan_decline_host_ns
                            .saturating_add(prepare_ns);
                        self.owner_group_work.plan_declines =
                            self.owner_group_work.plan_declines.saturating_add(1);
                    } else if hit {
                        self.owner_group_work.plan_hit_host_ns = self
                            .owner_group_work
                            .plan_hit_host_ns
                            .saturating_add(prepare_ns);
                        self.owner_group_work.plan_hits =
                            self.owner_group_work.plan_hits.saturating_add(1);
                    } else {
                        self.owner_group_work.plan_cold_host_ns = self
                            .owner_group_work
                            .plan_cold_host_ns
                            .saturating_add(prepare_ns);
                        self.owner_group_work.plan_cold =
                            self.owner_group_work.plan_cold.saturating_add(1);
                    }
                }
                self.owner_group_work.retained_payload_bytes =
                    group_plan.as_ref().map_or(0, |p| p.charged_bytes() as u64);
                self.owner_group_work.loops = self.owner_group_work.loops.saturating_add(1);
                self.owner_group_work.ordinals = self
                    .owner_group_work
                    .ordinals
                    .saturating_add(instructions.len() as u64);
            }
            let mut admitted = Vec::with_capacity(instructions.len());
            for (index, instruction) in instructions.iter().enumerate() {
                if !execution_visibility[index] {
                    admitted.push(false);
                    if self.owner_group_plan_diagnostics {
                        self.owner_group_work.execution_skipped =
                            self.owner_group_work.execution_skipped.saturating_add(1);
                    }
                    continue;
                }
                let cached = group_plan
                    .as_ref()
                    .and_then(|p| p.group_visible(index, services.show_soundings.then_some(33010)));
                let result = if let Some(group_visible) = cached {
                    if self.owner_group_plan_diagnostics {
                        self.owner_group_work.planned_decisions =
                            self.owner_group_work.planned_decisions.saturating_add(1);
                    }
                    // Re-evaluate original stroke and current scale; time/Parent/coverage remain above.
                    Ok(
                        ferrite_render::instruction_visible(instruction, viewing_scale, None, None)
                            && group_visible,
                    )
                } else {
                    if self.owner_group_plan_diagnostics {
                        self.owner_group_work.original_lookups =
                            self.owner_group_work.original_lookups.saturating_add(1);
                    }
                    // Unknown owners/errors are deferred until the original execution-true ordinal.
                    resources.instruction_visible_for_cell(
                        instruction,
                        viewing_scale,
                        services.show_soundings.then_some(33010),
                    )
                };
                match result {
                    Ok(visible) => {
                        admitted.push(visible);
                        if self.owner_group_plan_diagnostics {
                            if visible {
                                self.owner_group_work.admitted =
                                    self.owner_group_work.admitted.saturating_add(1);
                            } else {
                                self.owner_group_work.rejected =
                                    self.owner_group_work.rejected.saturating_add(1);
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(start) = owner_start {
                            self.owner_group_work.loop_host_ns =
                                self.owner_group_work.loop_host_ns.saturating_add(
                                    start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                                );
                        }
                        self.vector_frame.coverage_failed = true;
                        tracing::error!("Mixed-PC visibility rejected: {error}");
                        return Err(WgpuError::Render(format!(
                            "Mixed-PC visibility rejected: {error}"
                        )));
                    }
                }
            }
            if let Some(start) = owner_start {
                self.owner_group_work.loop_host_ns = self
                    .owner_group_work
                    .loop_host_ns
                    .saturating_add(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
            }
            Some(admitted)
        } else {
            None
        };
        drop(flat_owner_span);
        if let Some(start) = cpu_owner_start {
            services.record("emit_owner", start.elapsed());
        }
        // Both masks were freshly evaluated in this emitter invocation with this
        // source, scale and sounding override. Never a previous-frame permission.
        let current_admission_proves_visibility = self.current_frame_admission_reuse_enabled
            && (compact_frame.is_some() || owner_visibility.is_some());
        let execution_visibility = compact_frame
            .as_ref()
            .map(|f| f.mask.as_slice())
            .or(owner_visibility.as_deref())
            .unwrap_or(execution_visibility);
        // Suppression and emission receive this already intersected mask.
        // Reapplying a global union would grant or hide another PC's groups.
        let vector_groups = if resource_owners.is_some() {
            None
        } else {
            visible_viewing_groups
        };

        let cpu_view_deps_start = profiling.then(std::time::Instant::now);
        let flat_view_deps_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::ViewDependencyClassification,
        );
        self.vector_frame.view_dependent_symbols =
            if let Some(classification) = &self.vector_frame.static_source_classification {
                classification.requires_view_reprojection()
            } else {
                instructions.iter().any(|p| {
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
                })
            };
        drop(flat_view_deps_span);
        if let Some(start) = cpu_view_deps_start {
            services.record("emit_view_deps", start.elapsed());
        }
        let _instruction_count = instructions.len();
        let mut text_instruction_indices = execution
            .as_ref()
            .map(|_| vec![None; self.vector_frame.frame_cpu.text_labels.len()]);

        // Update viewport bounds for frustum culling
        self.update_viewport_bounds(services, &scaler);

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
        let tail_before = self
            .suppression_tail
            .as_ref()
            .map(|_| self.line_suppression.tail_diagnostics_counters());
        let tail_start = self
            .suppression_tail
            .as_ref()
            .map(|_| std::time::Instant::now());
        let suppressed_lines = self
            .line_suppression
            .plan_context_projected_with_visibility(
                context,
                self.viewing_scale(services),
                vector_groups,
                services.show_soundings.then_some(33010),
                Some(execution_visibility),
            );
        if let (Some(start), Some(before), Some(tail)) =
            (tail_start, tail_before, &mut self.suppression_tail)
        {
            let elapsed = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            tail.record(
                elapsed,
                viewing_scale,
                before,
                self.line_suppression.tail_diagnostics_counters(),
            );
        }
        if let Some(t) = suppression_timer {
            services.record("line_suppression", t.elapsed());
        }

        // Preparation-only mask. Original authoritative dispatch below is unchanged.
        // Moving this AFTER original suppression avoids projecting ranges which can
        // never acquire an unsuppressed original-source Entry.
        let source_batch_mask = if self.source_batch_parallel.enabled()
            && no_parent
            && source_projection_frame.is_some()
        {
            const FILTER_CAP: usize = 1024 * 1024;
            (|| {
                if execution_visibility.len() != instructions.len() {
                    return None;
                }
                let mut candidates = Vec::new();
                if instructions
                    .len()
                    .checked_mul(std::mem::size_of::<bool>())?
                    > FILTER_CAP
                {
                    return None;
                }
                candidates.try_reserve_exact(instructions.len()).ok()?;
                if candidates
                    .capacity()
                    .checked_mul(std::mem::size_of::<bool>())?
                    > FILTER_CAP
                {
                    return None;
                }
                let physical_scale = SCREEN_PX_PER_MM * services.state.scale_factor() as f32;
                for (ordinal, instruction) in instructions.iter().enumerate() {
                    let selected = if let DrawingInstruction::Line(line) = instruction {
                        execution_visibility[ordinal]
                            && crate::source_line_projection_arena::eligible(line)
                            && line.style.has_visible_stroke()
                            && line.style.physical_width(physical_scale) != 0.0
                            && suppressed_lines.spans(ordinal).is_none()
                            && source_bounds_frame
                                .as_ref()
                                .and_then(|bounds| bounds.peek_source_bounds(ordinal, &line.points))
                                .is_none_or(|bounds| {
                                    longitude_bounds_visible(
                                        bounds,
                                        self.vector_frame.viewport_world_bounds,
                                        self.vector_frame.lon_wrap_screen_px > 0.0,
                                    )
                                })
                    } else {
                        false
                    };
                    candidates.push(selected);
                }
                Some(candidates)
            })()
        } else {
            None
        };
        let source_batch = source_projection_frame.as_ref().and_then(|frame| {
            source_batch_mask.as_deref().and_then(|mask| {
                self.source_batch_parallel.prepare(
                    frame.projection_input(),
                    &scaler,
                    mask,
                    no_parent,
                )
            })
        });

        // Pre-compute world→screen transform once for all areas
        let area_transform = WgpuRenderer::scaler_transform(&scaler);

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

        // The earlier exact per-frame viewing_scale is shared by owner filtering and emission.

        // Retained hierarchy rejects groups of offscreen areas before geometry
        // key hashing, pattern/hatch preparation or ring scans. Original order,
        // suppression and exact culling inside the emitting routines are retained.
        let area_candidates = crate::area_candidates::Candidates::prepare(
            self.spatial_hierarchy_enabled,
            instructions.len(),
            self.dense_area_candidates_baseline,
            |area_candidates| {
                let index = context.scene_spatial_index();
                for id in index.areas.ids() {
                    area_candidates[id] = false;
                }
                index.areas.query_classified(
                    |b| {
                        longitude_bounds_relation(
                            (b.min[0], b.min[1], b.max[0], b.max[1]),
                            self.vector_frame.viewport_world_bounds,
                            self.vector_frame.lon_wrap_screen_px > 0.,
                        )
                    },
                    |id| area_candidates[id] = true,
                );
            },
        );

        if let Some(wave) = &mut emitter_wave {
            wave.begin_dispatch();
        }
        let cpu_dispatch_start = profiling.then(std::time::Instant::now);
        for inst_idx in crate::compact_owner_admission::Ordinals::new(
            instructions.len(),
            compact_frame.as_ref(),
        ) {
            let instruction = &instructions[inst_idx];
            if let Some(wave) = &mut emitter_wave {
                wave.visit(instruction);
            }
            let _flat_instruction_span =
                if self.flat_diagnostic.is_some() {
                    self.flat_span(services, match instruction {
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
            let line_timing = if matches!(instruction, DrawingInstruction::Line(_)) {
                self.line_preparation_work.clone()
            } else {
                None
            };
            if let Some(work) = &line_timing {
                let mut w = work.borrow_mut();
                w.line_ordinals_visited = w.line_ordinals_visited.saturating_add(1);
            }
            let line_admission = crate::line_preparation_diagnostics::span(
                line_timing.as_ref(),
                crate::line_preparation_diagnostics::Stage::Admission,
            );
            if !area_candidates.keeps(inst_idx) || !execution_visibility[inst_idx] {
                continue;
            }
            if !current_admission_proves_visibility
                && !ferrite_render::instruction_visible(
                    instruction,
                    viewing_scale,
                    vector_groups,
                    services.show_soundings.then_some(33010),
                )
            {
                continue;
            }

            drop(line_admission);
            let line_owner = crate::line_preparation_diagnostics::span(
                line_timing.as_ref(),
                crate::line_preparation_diagnostics::Stage::OwnerCoverage,
            );
            if let Some(cell) = &self.flat_diagnostic {
                let mut row = cell.borrow_mut();
                row.work.executed_commands = row.work.executed_commands.saturating_add(1);
            }
            let resolved = if let Some(resources) = resource_owners.as_deref_mut() {
                let cell = instruction.cell_index().map(|v| v as usize);
                let draw = if self.owner_draw_borrow_enabled {
                    resources
                        .resolve_draw_mut(cell)
                        .map(|value| (value.cache, value.profile))
                } else {
                    resources
                        .resolve_mut(cell)
                        .map(|value| (value.cache, value.profile))
                };
                match draw {
                    Ok(value) => Some(value),
                    Err(error) => {
                        self.vector_frame.coverage_failed = true;
                        tracing::error!("PC resource owner rejected: {error}");
                        return Err(WgpuError::Render(format!(
                            "PC resource owner rejected: {error}"
                        )));
                    }
                }
            } else {
                None
            };
            let (mut symbol_cache, color_profile) = match resolved {
                Some((cache, profile)) => (Some(cache), Some(profile)),
                None => (symbol_cache.as_deref_mut(), color_profile),
            };
            let inst_priority = instruction.priority().0;
            let inst_plane = instruction
                .display_plane()
                .composition_plane(CompositionStage::Chart);

            let instruction_source = (self.vector_frame.prepared_coverage.is_some()
                || self.is_device_fixed_source(services, inst_idx))
            .then_some(inst_idx);
            let same_coverage = (self.is_device_fixed_source(services, inst_idx)
                == current_coverage_source
                    .is_some_and(|s| self.is_device_fixed_source(services, s)))
                && match (
                    &self.vector_frame.prepared_coverage,
                    current_coverage_source,
                ) {
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
                    if self.vector_frame.frame_cpu.area_indices.len() > area_start_idx {
                        self.vector_frame.frame_cpu.area_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            area_start_idx,
                            self.vector_frame.frame_cpu.area_indices.len(),
                            current_coverage_source,
                        ));
                    }
                    area_start_idx = self.vector_frame.frame_cpu.area_indices.len();

                    // Record line range if any lines were added for previous group
                    if self.vector_frame.frame_cpu.line_geometry.index_len() > line_start_idx {
                        self.vector_frame.frame_cpu.line_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            line_start_idx,
                            self.vector_frame.frame_cpu.line_geometry.index_len(),
                            current_coverage_source,
                        ));
                    }
                    line_start_idx = self.vector_frame.frame_cpu.line_geometry.index_len();

                    // Record symbol range if any symbols were added for previous group
                    if self.vector_frame.frame_cpu.symbol_instances.len() > symbol_start_idx {
                        self.vector_frame.frame_cpu.symbol_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            symbol_start_idx,
                            self.vector_frame.frame_cpu.symbol_instances.len(),
                            current_coverage_source,
                        ));
                    }
                    symbol_start_idx = self.vector_frame.frame_cpu.symbol_instances.len();
                }
            }
            current_priority = Some(inst_priority);
            current_plane = inst_plane;
            current_coverage_source = instruction_source;
            self.vector_frame.emitting_coverage_source = instruction_source;
            if self.accepted_screen_line_packet.active() {
                self.accepted_screen_line_packet.material(
                    crate::accepted_screen_line_packet::Material {
                        ordinal: inst_idx,
                        primitive_kind: match instruction {
                            DrawingInstruction::Area(_) => 0,
                            DrawingInstruction::Line(_) => 1,
                            DrawingInstruction::Point(_) => 2,
                            DrawingInstruction::Text(_) => 3,
                        },
                        cell_index: instruction.cell_index(),
                        feature_id: instruction.feature_id(),
                        owner_resource_revision: symbol_cache
                            .as_ref()
                            .map_or(0, |cache| cache.resource_revision()),
                        coverage_source: instruction_source,
                        plane: instruction.display_plane().order().get(),
                        priority: instruction.priority().0,
                        ordinary_probe_eligible: false,
                    },
                );
            }

            // S-100 Scale-dependent visibility: skip instructions outside their scale range
            // Every surviving ordinal already passed the identical scale test:
            // either the current owner mask or the original dispatch predicate.
            if !self.current_frame_admission_reuse_enabled
                && !instruction.scale_range().is_visible_at(viewing_scale)
            {
                continue;
            }

            drop(line_owner);
            let inst_start = if profiling {
                Some(std::time::Instant::now())
            } else {
                None
            };

            let before_execution = execution.as_ref().map(|_| {
                (
                    self.vector_frame.frame_cpu.area_indices.len(),
                    self.vector_frame.frame_cpu.line_geometry.index_len(),
                    self.vector_frame.frame_cpu.pattern_indices.len(),
                    self.vector_frame.frame_cpu.symbol_instances.len(),
                    self.vector_frame.frame_cpu.text_labels.len(),
                )
            });
            match instruction {
                DrawingInstruction::Area(area) => {
                    let before = (
                        self.vector_frame.frame_cpu.area_indices.len(),
                        self.vector_frame.frame_cpu.pattern_indices.len(),
                        self.vector_frame.frame_cpu.symbol_instances.len(),
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
                            services.show_shallow_pattern,
                            symbol_cache
                                .as_ref()
                                .and_then(|cache| cache.shallow_pattern_contract()),
                        ) {
                            if let (Some(cache), Some(profile)) =
                                (symbol_cache.as_mut(), color_profile)
                            {
                                self.tile_area_with_pattern(
                                    services,
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
                            services,
                            area,
                            *color,
                            *width,
                            *spacing,
                            *angle,
                            &scaler,
                            inst_priority,
                        )?;
                    } else {
                        self.add_area_cached(services, area, area_transform);
                    }
                    if before
                        != (
                            self.vector_frame.frame_cpu.area_indices.len(),
                            self.vector_frame.frame_cpu.pattern_indices.len(),
                            self.vector_frame.frame_cpu.symbol_instances.len(),
                        )
                    {
                        self.vector_frame
                            .frame_cpu
                            .displayed_geometry
                            .push(inst_idx);
                    }
                    if let Some(s) = inst_start {
                        area_time += s.elapsed();
                        area_count += 1;
                    }
                }
                DrawingInstruction::Line(line) => {
                    let line_suppression = crate::line_preparation_diagnostics::span(
                        line_timing.as_ref(),
                        crate::line_preparation_diagnostics::Stage::SuppressionBounds,
                    );
                    // S-100 Part 9-11.1.9: Skip suppressed lines (lower-priority
                    // suppressible lines on curves already claimed by higher priority)
                    if suppressed_lines.contains(&inst_idx) {
                        _culled_count += 1;
                        continue;
                    }
                    let before = self.vector_frame.frame_cpu.line_geometry.index_len();
                    let bounds = self.static_line_bounds.prepare(inst_idx, line);
                    drop(line_suppression);
                    if self.accepted_screen_line_packet.active() {
                        self.accepted_screen_line_packet.material(
                            crate::accepted_screen_line_packet::Material {
                                ordinal: inst_idx,
                                primitive_kind: 1,
                                cell_index: line.cell_index,
                                feature_id: line.feature_id,
                                owner_resource_revision: symbol_cache
                                    .as_ref()
                                    .map_or(0, |cache| cache.resource_revision()),
                                coverage_source: self.vector_frame.emitting_coverage_source,
                                plane: line.display_plane.order().get(),
                                priority: line.priority.0,
                                ordinary_probe_eligible: suppressed_lines.spans(inst_idx).is_none()
                                    && crate::source_line_projection_arena::eligible(line),
                            },
                        );
                    }
                    let ordinary_ticket = emitter_wave
                        .as_mut()
                        .filter(|wave| wave.ordered_lines_enabled())
                        .and_then(|wave| {
                            let cpu = &self.vector_frame.frame_cpu.line_geometry;
                            wave.begin_ordinary_line(
                                inst_idx,
                                line,
                                suppressed_lines.spans(inst_idx).is_none(),
                                symbol_cache
                                    .as_ref()
                                    .map_or(0, |cache| cache.resource_revision()),
                                same_coverage,
                                [
                                    cpu.vertex_len(),
                                    cpu.index_len(),
                                    cpu.active_vertex_storage_bytes(),
                                    cpu.active_index_storage_bytes(),
                                ],
                            )
                        });
                    let line_result = self.add_line(
                        services,
                        context,
                        inst_idx,
                        line,
                        &scaler,
                        suppressed_lines.spans(inst_idx),
                        bounds,
                        source_projection_frame
                            .as_ref()
                            .map(|frame| (frame, inst_idx, source_bounds_frame.as_ref())),
                        source_batch.as_ref(),
                    );
                    if let (Some(wave), Some(ticket)) = (&mut emitter_wave, ordinary_ticket) {
                        let cpu = &self.vector_frame.frame_cpu.line_geometry;
                        wave.finish_ordinary_line(
                            ticket,
                            [
                                cpu.vertex_len(),
                                cpu.index_len(),
                                cpu.active_vertex_storage_bytes(),
                                cpu.active_index_storage_bytes(),
                            ],
                            line_result.is_ok(),
                        );
                    }
                    line_result?;
                    if self.vector_frame.frame_cpu.line_geometry.index_len() > before {
                        self.vector_frame
                            .frame_cpu
                            .displayed_geometry
                            .push(inst_idx);
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
                        && !self.is_point_visible(services, point.position.x, point.position.y)
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
                    let flags = self.get_symbol_flags(services, sym_id, &point.symbol_ref);
                    let is_sounding = flags & SYM_SOUNDING != 0;

                    // Soundings: respect show_soundings toggle
                    if is_sounding && !services.show_soundings {
                        continue;
                    }

                    // A fully culled point batch has no symbol-render attempt. Do not
                    // diagnose its resource/profile as missing merely because any=false.
                    // Device/reprojected anchors still bypass world-position culling.
                    if !points.iter().any(|point| {
                        point.portrayal_origin.requires_view_reprojection()
                            || self.is_point_visible(services, point.position.x, point.position.y)
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
                                    || self.is_point_visible(
                                        services,
                                        point.position.x,
                                        point.position.y,
                                    )
                                {
                                    any |= self.try_add_symbol(
                                        services, point, &scaler, cache, profile, sym_id,
                                    );
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
                        self.note_unrendered_symbol(
                            services,
                            symbol_cache
                                .as_ref()
                                .map_or(0, |cache| cache.resource_revision()),
                            sym_id,
                            &point.symbol_ref,
                        );
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
                        && !self.is_point_visible(services, text.position.x, text.position.y)
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
                    let dpi_scale = services.state.scale_factor() as f32;
                    let screen_dpi = 96.0 * dpi_scale;
                    let font_size_px = text.font_size * screen_dpi / 72.0;

                    // Apply offset (in mm from Lua LocalOffset, convert to pixels)
                    let offset_x = text.offset.x * SCREEN_PX_PER_MM * dpi_scale;
                    let offset_y = text.offset.y * SCREEN_PX_PER_MM * dpi_scale;
                    let sx = screen.x + offset_x;
                    let sy = screen.y - offset_y;

                    // The current draw cache already passed the original immutable
                    // cell-owner resolver above; resolve the font through that same owner.
                    let referenced_font =
                        if let Some(reference) = text.font_style.reference.as_deref() {
                            Some(
                                symbol_cache
                                    .ok_or_else(|| {
                                        WgpuError::Render(
                                            "FontReference has no PC resource owner".into(),
                                        )
                                    })?
                                    .resolve_font_reference(reference)?,
                            )
                        } else {
                            None
                        };
                    let font_family_override = referenced_font
                        .as_ref()
                        .map(crate::referenced_chart_owner::font_family);
                    self.vector_frame.frame_cpu.text_labels.push(TextLabel {
                        referenced_font,
                        font_family_override,
                        font_style: text.font_style.clone(),
                        source: self.vector_frame.emitting_coverage_source,
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
                        self.vector_frame.frame_cpu.area_indices.len(),
                        self.vector_frame.frame_cpu.line_geometry.index_len(),
                        self.vector_frame.frame_cpu.pattern_indices.len(),
                        self.vector_frame.frame_cpu.symbol_instances.len(),
                        self.vector_frame.frame_cpu.text_labels.len(),
                    );
                if matches!(instruction, DrawingInstruction::Point(_)) {
                    mask[inst_idx] = self.vector_frame.frame_cpu.symbol_instances[before.3..]
                        .iter()
                        .any(|instance| self.symbol_quad(services, instance).is_some());
                }
            }
        }

        if let Some(wave) = &mut emitter_wave {
            wave.end_dispatch([
                self.vector_frame.frame_cpu.area_vertices.len(),
                self.vector_frame.frame_cpu.line_geometry.vertex_len(),
                self.vector_frame.frame_cpu.line_geometry.index_len(),
                self.vector_frame.frame_cpu.symbol_instances.len(),
                self.vector_frame.frame_cpu.text_labels.len(),
            ]);
        }
        if let Some(start) = cpu_dispatch_start {
            services.record("emit_dispatch_inclusive", start.elapsed());
        }
        let uses_references = self
            .vector_frame
            .frame_cpu
            .text_labels
            .iter()
            .any(|label| label.referenced_font.is_some());
        let mut next_chart_owner = if uses_references || services.fonts.is_private() {
            // Independent owner is used for ALL labels so collision/atlas namespace is shared.
            crate::referenced_chart_owner::validate_all_text(
                self.vector_frame
                    .frame_cpu
                    .text_labels
                    .iter()
                    .map(|label| (label.text.as_str(), label.font_size)),
            )?;
            services.fonts.ensure_font_metrics(&services.state.window);
            let extent = [services.state.size.width, services.state.size.height];
            let density = services.state.scale_factor() as f32;
            let requests = || {
                self.vector_frame
                    .frame_cpu
                    .text_labels
                    .iter()
                    .filter_map(|label| {
                        label
                            .referenced_font
                            .as_ref()
                            .map(|font| (font, label.text.as_str()))
                    })
            };
            let reusable = self
                .referenced_chart_owner
                .as_ref()
                .map(|owner| {
                    owner.matches(
                        extent,
                        density,
                        services.fonts.context().pixels_per_point(),
                        requests(),
                    )
                })
                .transpose()?
                .unwrap_or(false);
            if reusable {
                let owner = self.referenced_chart_owner.take().ok_or_else(|| {
                    WgpuError::Render("Referenced chart font owner disappeared".into())
                })?;
                owner.begin_metrics();
                Some(owner)
            } else {
                Some(
                    crate::referenced_chart_owner::ReferencedChartOwner::prepare(
                        &services.state.device,
                        services.state.config.format,
                        services.fonts.context(),
                        extent,
                        density,
                        requests(),
                    )?,
                )
            }
        } else {
            None
        };
        if let (Some(indices), Some(mask)) = (text_instruction_indices, execution) {
            if !self.vector_frame.frame_cpu.text_labels.is_empty() {
                services.fonts.ensure_font_metrics(&services.state.window);
                let (_, accepted) = self.layout_chart_text_with_owner(
                    services,
                    next_chart_owner.as_ref(),
                    Vec::new(),
                    false,
                );
                for (i, source) in indices.into_iter().enumerate() {
                    if let Some(source) = source {
                        mask[source] = accepted[i];
                    }
                }
            }
        }

        if let Some(owner) = &mut next_chart_owner {
            owner.end_metrics()?;
        }
        self.referenced_chart_owner = next_chart_owner;

        // Log per-type instruction timing
        if profiling {
            services.record("inst_area", area_time);
            services.record("inst_line", line_time);
            services.record("inst_symbol", symbol_time);
            services.record("inst_text", text_time);
            tracing::debug!(
                "[PROFILER] Instructions: area={} ({:.2}ms), line={} ({:.2}ms), symbol={} ({:.2}ms), text={} ({:.2}ms)",
                area_count,
                area_time.as_secs_f64() * 1000.0,
                line_count,
                line_time.as_secs_f64() * 1000.0,
                symbol_count,
                symbol_time.as_secs_f64() * 1000.0,
                text_count,
                text_time.as_secs_f64() * 1000.0,
            );
        }

        if let Some(t) = total_timer {
            let elapsed = t.elapsed();
            services.record("add_instructions_total", elapsed);
            tracing::debug!(
                "[PROFILER] add_instructions_total: {:.2}ms (areas: {}v/{}i, lines: {}v/{}i, symbols: {}, texts: {})",
                elapsed.as_secs_f64() * 1000.0,
                self.vector_frame.frame_cpu.area_vertices.len(),
                self.vector_frame.frame_cpu.area_indices.len(),
                self.vector_frame.frame_cpu.line_geometry.vertex_len(),
                self.vector_frame.frame_cpu.line_geometry.index_len(),
                self.vector_frame.frame_cpu.symbol_instances.len(),
                self.vector_frame.frame_cpu.text_labels.len(),
            );
        }

        // Record final priority ranges
        if let Some(final_priority) = current_priority {
            if self.vector_frame.frame_cpu.area_indices.len() > area_start_idx {
                self.vector_frame.frame_cpu.area_priority_ranges.push((
                    current_plane,
                    final_priority,
                    area_start_idx,
                    self.vector_frame.frame_cpu.area_indices.len(),
                    current_coverage_source,
                ));
            }
            if self.vector_frame.frame_cpu.line_geometry.index_len() > line_start_idx {
                self.vector_frame.frame_cpu.line_priority_ranges.push((
                    current_plane,
                    final_priority,
                    line_start_idx,
                    self.vector_frame.frame_cpu.line_geometry.index_len(),
                    current_coverage_source,
                ));
            }
            if self.vector_frame.frame_cpu.symbol_instances.len() > symbol_start_idx {
                self.vector_frame.frame_cpu.symbol_priority_ranges.push((
                    current_plane,
                    final_priority,
                    symbol_start_idx,
                    self.vector_frame.frame_cpu.symbol_instances.len(),
                    current_coverage_source,
                ));
            }
        }
        // Invalidate intermediate/final picking envelopes without projecting every
        // polygon again. The first actual pick builds only the final draw list.
        self.vector_frame.selection_index.take();
        drop(source_batch);
        self.vector_frame.temporal_visibility_mask = temporal_visible;
        self.vector_frame.emitting_coverage_source = None;
        self.accepted_screen_line_packet.finish([
            self.vector_frame.frame_cpu.line_geometry.vertex_len(),
            self.vector_frame.frame_cpu.line_geometry.index_len(),
        ]);
        Ok(())
    }
    fn ensure_triangulated(
        &mut self,
        _services: &mut EmissionServices<'_>,
        area: &ferrite_render::AreaInstruction,
        projection: ferrite_render::FlatProjection,
    ) -> Option<(usize, usize, ferrite_render::FlatProjection)> {
        let cache_key = WgpuRenderer::area_geometry_key(area, projection);

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
    fn add_area_cached(
        &mut self,
        services: &mut EmissionServices<'_>,
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

        let cache_key = WgpuRenderer::area_geometry_key(area, transform.projection);

        // Fast path: triangulation already cached — use cached AABB for O(1) frustum culling
        // (avoids re-scanning entire exterior ring just to compute AABB)
        if let Some(cached) = self.triangulation_cache.get(&cache_key) {
            // Frustum culling with cached AABB
            if let Some((vp_min_x, vp_min_y, vp_max_x, vp_max_y)) =
                self.vector_frame.viewport_world_bounds
            {
                let (ax, ay, bx, by) = cached.world_aabb;
                let margin_x = (vp_max_x - vp_min_x) * 0.5;
                let margin_y = (vp_max_y - vp_min_y) * 0.5;
                let copies = if self.vector_frame.lon_wrap_screen_px > 0. {
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

            let base_index = self.vector_frame.frame_cpu.area_vertices.len() as u32;
            let [scale_x, scale_y] = transform.scale;
            let [offset_x, offset_y] = transform.offset;
            let [min_x, max_lat] = transform.geographic_origin;
            let max_y = transform.projection.project_y(max_lat);

            let memoized = match self.triangulation_cache.get(&cache_key) {
                Some(TriangulationStorage::Shared(owner)) => {
                    let owned_bytes = owner
                        .world_vertices
                        .capacity()
                        .checked_mul(std::mem::size_of::<f64>())
                        .and_then(|n| {
                            owner
                                .indices
                                .capacity()
                                .checked_mul(std::mem::size_of::<usize>())
                                .and_then(|i| n.checked_add(i))
                        })
                        .and_then(|n| n.checked_add(std::mem::size_of::<CachedTriangulation>()));
                    owned_bytes.and_then(|bytes| {
                        self.area_projection_shadow
                            .project(owner, wv, bytes, transform, color)
                    })
                }
                _ => None,
            };
            if let Some(vertices) = memoized {
                self.vector_frame
                    .frame_cpu
                    .area_vertices
                    .extend_from_slice(&vertices);
            } else {
                self.vector_frame
                    .frame_cpu
                    .area_vertices
                    .reserve(total_vertex_count);
                self.vector_frame
                    .frame_cpu
                    .area_vertices
                    .extend((0..total_vertex_count).map(|i| {
                        let wx = wv[i * 2];
                        let wy = wv[i * 2 + 1];
                        let sx = ((wx - min_x) * scale_x + offset_x) as f32;
                        let sy = ((max_y - wy) * scale_y + offset_y) as f32;
                        Vertex2D::new(sx, sy, color)
                    }));
            }
            self.vector_frame.frame_cpu.area_indices.reserve(idx_len);
            self.vector_frame
                .frame_cpu
                .area_indices
                .extend(indices.iter().map(|&i| base_index + i as u32));

            // Retain immutable projected input only after the original emitter
            // has passed visibility, clipping/coverage and triangulation gates.
            // Other material families and device-fixed origins remain legacy.
            if matches!(
                area.portrayal_origin,
                ferrite_render::PortrayalOrigin::NonPoint
            ) {
                self.retained_world_areas
                    .capture(wv, color, transform, base_index as usize);
            } else {
                self.retained_world_areas.reject_source();
            }
            return;
        }

        // Cold path: first-time triangulation — fall back to ring-scan culling
        if !WgpuRenderer::is_ring_visible_static(
            &area.exterior,
            self.vector_frame.viewport_world_bounds,
            self.vector_frame.lon_wrap_screen_px > 0.0,
        ) {
            return;
        }

        // Ensure triangulation is cached
        if self
            .ensure_triangulated(services, area, transform.projection)
            .is_none()
        {
            return;
        }

        // Recurse once: now the cache is populated, fast path will handle it
        self.add_area_cached(services, area, transform);
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Mechanical shared emitter preserves independent authority, coverage and ownership inputs"
    )]
    fn tile_area_with_pattern(
        &mut self,
        services: &mut EmissionServices<'_>,
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
        if !WgpuRenderer::is_ring_visible_static(
            &area.exterior,
            self.vector_frame.viewport_world_bounds,
            self.vector_frame.lon_wrap_screen_px > 0.0,
        ) {
            return;
        }

        // Apply HiDPI scale factor so pattern matches physical mm on screen
        let dpi_scale = services.state.scale_factor() as f32;
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
        let pat_key = format!(
            "{}:{}",
            symbol_cache.resource_revision(),
            crate::symbol_cache::pattern_texture_key(
                symbol_ref,
                spacing_x_px,
                spacing_y_px,
                mm_to_px,
            )
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
            let (texture, view) = services.state.create_texture_from_rgba(
                &geom.pixels,
                tex_w,
                tex_h,
                &format!("pattern_{}", symbol_ref),
            );
            let bind_group = services
                .pipelines
                .create_pattern_bind_group(&services.state.device, &view);
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
        let Some(key) = self.ensure_triangulated(services, area, scaler.projection()) else {
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
        let idx_start = self.vector_frame.frame_cpu.pattern_indices.len();
        let vertex_start = self.vector_frame.frame_cpu.pattern_vertices.len();
        let plane = area
            .display_plane
            .composition_plane(CompositionStage::Chart);
        if clipped {
            self.vector_frame.view_clipped_patterns = true;
            let v = scaler.viewport;
            let rect = [
                v.x as f64 - 2.,
                v.y as f64 - 2.,
                (v.x + v.width) as f64 + 2.,
                (v.y + v.height) as f64 + 2.,
            ];
            let wraps = if self.vector_frame.lon_wrap_screen_px > 0. {
                vec![0., -(360. * sx), 360. * sx]
            } else {
                vec![0.]
            };
            for (wrap_mode, dx) in wraps.into_iter().enumerate() {
                let start = self.vector_frame.frame_cpu.pattern_indices.len();
                let clipped_vertex_start = self.vector_frame.frame_cpu.pattern_vertices.len();
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
                    let base = self.vector_frame.frame_cpu.pattern_vertices.len() as u32;
                    self.vector_frame
                        .frame_cpu
                        .pattern_vertices
                        .extend(points.iter().map(|p| {
                            PatternVertex::new(
                                p[0] as f32,
                                p[1] as f32,
                                inv_tx,
                                inv_ty,
                                shear_screen,
                            )
                        }));
                    for i in 1..points.len() - 1 {
                        self.vector_frame
                            .frame_cpu
                            .pattern_indices
                            .extend_from_slice(&[base, base + i as u32, base + i as u32 + 1]);
                    }
                }
                let end = self.vector_frame.frame_cpu.pattern_indices.len();
                if let Some(records) = self.vector_frame.frame_cpu.pattern_emission_audit.as_mut() {
                    record_pattern_emission(
                        records,
                        &mut self.vector_frame.frame_cpu.pattern_emission_audit_dropped,
                        PatternEmissionAudit {
                            source_ordinal,
                            vertex_start: clipped_vertex_start,
                            vertex_end: self.vector_frame.frame_cpu.pattern_vertices.len(),
                            index_start: start,
                            index_end: end,
                            wrap_mode: wrap_mode as u8,
                            wrap_dx_screen_bits: dx.to_bits(),
                        },
                    );
                }
                self.vector_frame.frame_cpu.pattern_ranges.push((
                    plane,
                    priority,
                    start,
                    end,
                    pat_key.clone(),
                    wrap_mode as u8,
                    self.vector_frame.emitting_coverage_source,
                ));
            }
        } else {
            let base = self.vector_frame.frame_cpu.pattern_vertices.len() as u32;
            self.vector_frame.frame_cpu.pattern_vertices.extend(
                coords.as_chunks::<2>().0.iter().map(|p| {
                    PatternVertex::new(p[0] as f32, p[1] as f32, inv_tx, inv_ty, shear_screen)
                }),
            );
            self.vector_frame
                .frame_cpu
                .pattern_indices
                .extend(indices.iter().map(|i| base + *i as u32));
        }
        let idx_end = self.vector_frame.frame_cpu.pattern_indices.len();
        let plane = area
            .display_plane
            .composition_plane(CompositionStage::Chart);
        if !clipped {
            if let Some(records) = self.vector_frame.frame_cpu.pattern_emission_audit.as_mut() {
                record_pattern_emission(
                    records,
                    &mut self.vector_frame.frame_cpu.pattern_emission_audit_dropped,
                    PatternEmissionAudit {
                        source_ordinal,
                        vertex_start,
                        vertex_end: self.vector_frame.frame_cpu.pattern_vertices.len(),
                        index_start: idx_start,
                        index_end: idx_end,
                        wrap_mode: 255,
                        wrap_dx_screen_bits: 0_f64.to_bits(),
                    },
                );
            }
            self.vector_frame.frame_cpu.pattern_ranges.push((
                plane,
                priority,
                idx_start,
                idx_end,
                pat_key,
                255,
                self.vector_frame.emitting_coverage_source,
            ));
        }
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Mechanical shared emitter preserves independent authority, coverage and ownership inputs"
    )]
    fn tile_area_with_hatch(
        &mut self,
        services: &mut EmissionServices<'_>,
        area: &ferrite_render::AreaInstruction,
        color: Color,
        width: f32,
        spacing_mm: f32,
        angle_deg: f32,
        scaler: &ferrite_render::Scaler,
        _priority: i32,
    ) -> crate::Result<()> {
        // Frustum culling: quick AABB check on exterior ring
        if !WgpuRenderer::is_ring_visible_static(
            &area.exterior,
            self.vector_frame.viewport_world_bounds,
            self.vector_frame.lon_wrap_screen_px > 0.0,
        ) {
            return Ok(());
        }

        let dpi_scale = services.state.scale_factor() as f32;
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
            return Ok(());
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
        let (vp_w, vp_h) = services.state.viewport_size();
        let hatch_clip_min_x = -hatch_margin;
        let hatch_clip_min_y = -hatch_margin;
        let hatch_clip_max_x = vp_w + hatch_margin;
        let hatch_clip_max_y = vp_h + hatch_margin;
        let half_line_width = line_width * 0.5;

        self.accepted_screen_line_packet.path();
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
            let segments = WgpuRenderer::clip_line_to_polygon(lx0, ly0, lx1, ly1, &screen_ring);
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
                self.accepted_screen_line_packet.record(
                    [sx, sy, ex, ey],
                    [
                        hatch_clip_min_x,
                        hatch_clip_min_y,
                        hatch_clip_max_x,
                        hatch_clip_max_y,
                    ],
                    line_width,
                    color_arr,
                    [
                        self.vector_frame.frame_cpu.line_geometry.vertex_len(),
                        self.vector_frame.frame_cpu.line_geometry.index_len(),
                    ],
                );
                let nx = -ldy / len * half_line_width;
                let ny = ldx / len * half_line_width;

                self.vector_frame
                    .frame_cpu
                    .line_geometry
                    .append_emitted([sx, sy], [ex, ey], [-nx, -ny], [nx, ny], color_arr)
                    .map_err(|e| crate::WgpuError::Render(e.into()))?;
            }

            d += spacing_px;
        }
        Ok(())
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Original serial emission plus source-only batch input"
    )]
    fn add_line(
        &mut self,
        services: &mut EmissionServices<'_>,
        context: &RenderContext,
        ordinal: usize,
        line: &ferrite_render::LineInstruction,
        scaler: &ferrite_render::Scaler,
        spans: Option<&[ferrite_render::LineSpan]>,
        retained_bounds: Option<(f64, f64, f64, f64)>,
        source_projection: Option<crate::source_line_projection_arena::SourceProjection<'_>>,
        source_batch: Option<&crate::source_batch_parallel::Batch<'_>>,
    ) -> crate::Result<()> {
        if !line.style.has_visible_stroke() {
            return Ok(());
        }
        let materialize = crate::line_preparation_diagnostics::span(
            self.line_preparation_work.as_ref(),
            crate::line_preparation_diagnostics::Stage::Paths,
        );
        let paths = context
            .resolved_line_paths(ordinal, scaler)
            .unwrap_or_else(|| line.render_paths(scaler));
        drop(materialize);
        for points in paths {
            let bounds = std::ptr::eq(points.as_ref(), line.points.as_slice())
                .then_some(retained_bounds)
                .flatten();
            let bounds = bounds.or_else(|| {
                if spans.is_none() {
                    source_projection.and_then(|(_, ordinal, bound)| {
                        bound.and_then(|bound| bound.source_bounds(ordinal, &points))
                    })
                } else {
                    None
                }
            });
            // Styled, dynamic and partially suppressed paths use the unchanged fallback.
            let entry = if spans.is_none()
                && crate::source_line_projection_arena::eligible(line)
                && std::ptr::eq(points.as_ref(), line.points.as_slice())
            {
                source_projection.and_then(|(frame, ordinal, _)| frame.entry(ordinal, &points))
            } else {
                None
            };
            self.add_line_points(
                services,
                line,
                scaler,
                &points,
                spans,
                bounds,
                entry,
                source_batch,
            )?;
        }
        Ok(())
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Mechanical shared emitter preserves independent authority, coverage and ownership inputs"
    )]
    fn add_line_points(
        &mut self,
        services: &mut EmissionServices<'_>,
        line: &ferrite_render::LineInstruction,
        scaler: &ferrite_render::Scaler,
        points: &[ferrite_render::WorldPoint],
        spans: Option<&[ferrite_render::LineSpan]>,
        retained_bounds: Option<(f64, f64, f64, f64)>,
        source_projection: Option<crate::source_line_projection_arena::Entry<'_>>,
        source_batch: Option<&crate::source_batch_parallel::Batch<'_>>,
    ) -> crate::Result<()> {
        let timing = self.line_preparation_work.clone();
        let gate = crate::line_preparation_diagnostics::span(
            timing.as_ref(),
            crate::line_preparation_diagnostics::Stage::GateStyle,
        );
        if let Some(work) = &timing {
            let mut w = work.borrow_mut();
            w.source_path_points = w.source_path_points.saturating_add(points.len() as u64);
        }
        self.accepted_screen_line_packet.path();
        let vertex_start = self.vector_frame.frame_cpu.line_geometry.vertex_len();
        if points.len() < 2 {
            return Ok(());
        }

        // Source AABB alone cannot cull a screen-offset line.
        if line.style.offset_mm == 0. {
            let (ax, ay, bx, by) = match retained_bounds {
                Some(bounds) => bounds,
                None => {
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
                    (ax, ay, bx, by)
                }
            };
            if !self.is_aabb_visible(services, ax, ay, bx, by) {
                return Ok(());
            }
        }

        let color = line.style.color.to_array();
        let width = line
            .style
            .physical_width(SCREEN_PX_PER_MM * services.state.scale_factor() as f32);
        if width == 0.0 {
            return Ok(());
        }

        let offset_px =
            line.style.offset_mm * (SCREEN_PX_PER_MM * services.state.scale_factor() as f32) as f64;
        drop(gate);
        let offsets = if offset_px == 0. {
            None
        } else {
            let project_clock = crate::line_preparation_diagnostics::span(
                timing.as_ref(),
                crate::line_preparation_diagnostics::Stage::OffsetProjection,
            );
            let projected: Vec<_> = points
                .iter()
                .map(|p| {
                    let s = scaler.world_to_screen(*p);
                    [s.x as f64, s.y as f64]
                })
                .collect();
            drop(project_clock);
            let _offset_clock = crate::line_preparation_diagnostics::span(
                timing.as_ref(),
                crate::line_preparation_diagnostics::Stage::OffsetSolver,
            );
            match ferrite_kernel::line_offset::screen_line_offsets(
                &projected,
                offset_px,
                projected.len() > 2 && projected.first() == projected.last(),
            ) {
                Ok(offsets) => Some(offsets),
                Err(error) => {
                    tracing::warn!("Physical line offset: {}", error);
                    return Ok(());
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
        let dash_clock = crate::line_preparation_diagnostics::span(
            timing.as_ref(),
            crate::line_preparation_diagnostics::Stage::Dash,
        );
        let styled = ferrite_render::dash_line_spans(points, scaler, &line.style, spans);
        drop(dash_clock);
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
        let before_emission = timing.as_ref().map(|_| {
            (
                self.vector_frame.frame_cpu.line_geometry.vertex_len(),
                self.vector_frame.frame_cpu.line_geometry.index_len(),
                self.vector_frame
                    .frame_cpu
                    .line_geometry
                    .active_vertex_storage_bytes(),
                self.vector_frame
                    .frame_cpu
                    .line_geometry
                    .active_index_storage_bytes(),
            )
        });
        let emission_clock = crate::line_preparation_diagnostics::span(
            timing.as_ref(),
            crate::line_preparation_diagnostics::Stage::ProjectionMesh,
        );
        if let Some(spans) = spans {
            for span in spans {
                if let Some((a, b)) = span.screen_endpoints(points, scaler) {
                    self.add_line_span(
                        services,
                        shifted(a, span.segment, span.start),
                        shifted(b, span.segment, span.end),
                        color,
                        width,
                        clip,
                    )?;
                }
            }
        } else {
            // Only original immutable world paths. Fixed-mm/dynamic paths,
            // offsets, dashes and partial suppression keep their original path.
            let prepared = if source_projection.is_none()
                && line.screen_ray.is_none()
                && line.portrayal_path.is_none()
                && matches!(
                    line.portrayal_origin,
                    ferrite_render::PortrayalOrigin::NonPoint
                )
                && offset_px == 0.0
                && line.style.dash_cycle.is_none()
                && line.style.dash_pattern.is_empty()
                && std::ptr::eq(points, line.points.as_slice())
            {
                self.moving_line_northing.prepare(points, scaler)
            } else {
                None
            };
            let project = |index: usize, point: WorldPoint| {
                if let Some(entry) = &source_projection {
                    if let Some(projected) =
                        source_batch.and_then(|batch| batch.project(*entry, index, point, scaler))
                    {
                        return projected;
                    }
                    return entry.project(index, point, scaler);
                }
                prepared.as_ref().map_or_else(
                    || scaler.world_to_screen(point),
                    |entry| entry.project(index, point, scaler),
                )
            };
            let mut prev = shifted(project(0, points[0]), 0, 0.);
            for (segment, p) in points[1..].iter().enumerate() {
                let curr = shifted(project(segment + 1, *p), segment, 1.);
                self.add_line_span(services, prev, curr, color, width, clip)?;
                prev = curr;
            }
        }
        drop(emission_clock);
        if let (Some(work), Some((old_vertices, old_indices, old_v_capacity, old_i_capacity))) =
            (&timing, before_emission)
        {
            let mut w = work.borrow_mut();
            w.emitted_vertices = w.emitted_vertices.saturating_add(
                (self.vector_frame.frame_cpu.line_geometry.vertex_len() - old_vertices) as u64,
            );
            w.emitted_indices = w.emitted_indices.saturating_add(
                (self.vector_frame.frame_cpu.line_geometry.index_len() - old_indices) as u64,
            );
            w.vertex_capacity_growth_bytes = w.vertex_capacity_growth_bytes.saturating_add(
                self.vector_frame
                    .frame_cpu
                    .line_geometry
                    .active_vertex_storage_bytes()
                    .saturating_sub(old_v_capacity) as u64,
            );
            w.index_capacity_growth_bytes = w.index_capacity_growth_bytes.saturating_add(
                self.vector_frame
                    .frame_cpu
                    .line_geometry
                    .active_index_storage_bytes()
                    .saturating_sub(old_i_capacity) as u64,
            );
        }
        if line.screen_ray.is_some() || line.portrayal_path.is_some() {
            let _anchor_clock = crate::line_preparation_diagnostics::span(
                timing.as_ref(),
                crate::line_preparation_diagnostics::Stage::Anchor,
            );
            let anchor = scaler.world_to_screen(line.points[0]);
            self.vector_frame
                .frame_cpu
                .line_geometry
                .reanchor_suffix(vertex_start, [anchor.x, anchor.y])
                .map_err(|e| crate::WgpuError::Render(e.into()))?;
        }
        Ok(())
    }
    fn add_line_span(
        &mut self,
        _services: &mut EmissionServices<'_>,
        prev: ferrite_render::ScreenPoint,
        curr: ferrite_render::ScreenPoint,
        color: [f32; 4],
        width: f32,
        clip: [f32; 4],
    ) -> crate::Result<()> {
        let [clip_x_min, clip_y_min, clip_x_max, clip_y_max] = clip;
        // Skip segments with NaN/Inf coordinates
        if !prev.x.is_finite() || !prev.y.is_finite() || !curr.x.is_finite() || !curr.y.is_finite()
        {
            return Ok(());
        }

        // Clip to viewport bounds for clean edges
        if let Some((cx0, cy0, cx1, cy1)) = WgpuRenderer::clip_line_segment(
            prev.x, prev.y, curr.x, curr.y, clip_x_min, clip_y_min, clip_x_max, clip_y_max,
        ) {
            let dx = cx1 - cx0;
            let dy = cy1 - cy0;
            let len = (dx * dx + dy * dy).sqrt();

            if len >= 0.001 {
                self.accepted_screen_line_packet.record(
                    [cx0, cy0, cx1, cy1],
                    clip,
                    width,
                    color,
                    [
                        self.vector_frame.frame_cpu.line_geometry.vertex_len(),
                        self.vector_frame.frame_cpu.line_geometry.index_len(),
                    ],
                );
                let nx = -dy / len * width * 0.5;
                let ny = dx / len * width * 0.5;

                self.vector_frame
                    .frame_cpu
                    .line_geometry
                    .append_emitted([cx0, cy0], [cx1, cy1], [-nx, -ny], [nx, ny], color)
                    .map_err(|e| crate::WgpuError::Render(e.into()))?;
            }
        }
        Ok(())
    }
    fn get_symbol_flags(
        &mut self,
        _services: &mut EmissionServices<'_>,
        symbol_id: SymbolId,
        symbol_str: &str,
    ) -> u8 {
        if let Some(&flags) = self.symbol_class_cache.get(&symbol_id) {
            return flags;
        }
        let flags = classify_symbol(symbol_str);
        self.symbol_class_cache.insert(symbol_id, flags);
        flags
    }
    fn ensure_symbol_texture(
        &mut self,
        services: &mut EmissionServices<'_>,
        symbol_id: (u64, SymbolId),
        symbol_str: &str,
        geom: &crate::SymbolGeometry,
    ) {
        self.symbol_textures.entry(symbol_id).or_insert_with(|| {
            create_symbol_texture(services.state, services.pipelines, symbol_str, geom)
        });
    }
    fn try_add_symbol(
        &mut self,
        services: &mut EmissionServices<'_>,
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
        let resource_owner = symbol_cache.resource_revision();
        let geom = match symbol_cache.get_symbol(symbol_str, color_profile) {
            Some(g) => g,
            None => return false,
        };

        self.ensure_symbol_texture(services, (resource_owner, symbol_id), symbol_str, geom);

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
        let mm_to_px = (96.0 / 25.4) * services.state.scale_factor() as f32;
        screen.x += point.local_offset.0 * mm_to_px;
        screen.y -= point.local_offset.1 * mm_to_px;

        // Preserve PC order, every original point member and all of its symbols
        // (digits, quality marks, drying/negative signs). Even coincident commands
        // can have different sources, planes or scales, and must not be merged.
        // Add symbol instance for rendering (uses interned SymbolId - 4 bytes vs 24+ for String)
        self.vector_frame
            .frame_cpu
            .symbol_instances
            .push(SymbolInstance {
                source: self.vector_frame.emitting_coverage_source,
                plane: point
                    .display_plane
                    .composition_plane(CompositionStage::Chart),
                feature_id: point.feature_id,
                cell_index: point.cell_index,
                world: point.position,
                priority: point.priority.0,
                anchor,
                symbol_id,
                resource_owner,
                screen_x: screen.x,
                screen_y: screen.y,
                scale: point.scale,
                rotation,
            });

        true
    }
    fn note_unrendered_symbol(
        &mut self,
        _services: &mut EmissionServices<'_>,
        owner: u64,
        sym_id: SymbolId,
        symbol_ref: &str,
    ) {
        if symbol_ref.is_empty() {
            self.empty_symbol_ref_count = self.empty_symbol_ref_count.saturating_add(1);
            return;
        }
        if self.missing_symbol_ids.insert((owner, sym_id)) {
            tracing::warn!(
                "Symbol '{}' could not be rendered (missing SVG or unresolved color tokens) — \
                 check Portrayal Catalogue completeness",
                symbol_ref
            );
        }
    }
    fn layout_chart_text_with_owner(
        &self,
        services: &mut EmissionServices<'_>,
        owner: Option<&crate::referenced_chart_owner::ReferencedChartOwner>,
        shapes: Vec<ChartTextShape>,
        capture_shapes: bool,
    ) -> (Vec<ChartTextShape>, Vec<bool>) {
        let _flat_text_span = self.flat_span(
            services,
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::TextAndDeclutter,
        );
        let (width, height) = services.state.viewport_size();
        layout_chart_text_with_fonts(
            ChartTextLayoutEnvironment {
                physical_extent: [width, height],
                pan: [
                    self.vector_frame.screen_pan_offset.0,
                    self.vector_frame.screen_pan_offset.1,
                ],
                zoom: [
                    self.vector_frame.screen_zoom_scale,
                    self.vector_frame.screen_zoom_scale_y,
                ],
                pivot: [
                    self.vector_frame.screen_zoom_pivot.0,
                    self.vector_frame.screen_zoom_pivot.1,
                ],
                longitude_wrap: self.vector_frame.lon_wrap_screen_px,
                source_classification: self.vector_frame.static_source_classification.as_deref(),
                device_fixed_sources: &self.vector_frame.device_fixed_sources,
            },
            owner.map_or(services.fonts.context(), |owner| &owner.context),
            &self.vector_frame.frame_cpu.text_labels,
            shapes,
            capture_shapes,
        )
    }
}

type EmittedSymbolBuffers = Vec<(
    CompositionPlane,
    i32,
    usize,
    usize,
    wgpu::Buffer,
    wgpu::Buffer,
    Vec<((u64, SymbolId), u32, u32)>,
)>;
impl VectorEmissionOwned {
    fn fork_private(&self) -> Self {
        Self {
            scene_draw_plan: self.scene_draw_plan.fork_cold(),
            primary_line_quad_enabled: self.primary_line_quad_enabled,
            accepted_screen_line_packet: self.accepted_screen_line_packet.fork_cold(),
            area_projection_shadow: crate::retained_world_area::ExactAreaShadowCache::new(
                self.retained_world_areas.enabled(),
            ),
            area_triangulation_reuse_enabled: self.area_triangulation_reuse_enabled,
            cached_symbol_buffers: Vec::new(),
            compact_owner_admission_diagnostics: self.compact_owner_admission_diagnostics,
            compact_owner_admission_enabled: self.compact_owner_admission_enabled,
            current_frame_admission_reuse_enabled: self.current_frame_admission_reuse_enabled,
            compact_owner_work: Default::default(),
            coverage_pipelines: self.coverage_pipelines.as_ref().map(Arc::clone),
            coverage_trial_reuse: self.coverage_trial_reuse,
            coverage_trial_work: Default::default(),
            emitter_wave_work: None,
            empty_symbol_ref_count: 0,
            flat_diagnostic: None,
            flat_gpu_coverage_host_ns: 0,
            flat_stage_timing_enabled: self.flat_stage_timing_enabled,
            frame_local_coverage_clip_reuse: self.frame_local_coverage_clip_reuse,
            gpu_buffers_dirty: true,
            gpu_timestamp_requested: false,
            gpu_timestamp_target_camera: None,
            line_preparation_work: None,
            line_suppression: self.line_suppression.fork_empty_with_same_policy(),
            missing_symbol_ids: FxHashSet::with_capacity_and_hasher(32, Default::default()),
            moving_line_northing: self.moving_line_northing.fork_cold(),
            source_batch_parallel: self.source_batch_parallel.fork_cold(),
            native_route_target_camera: None,
            owner_draw_borrow_enabled: self.owner_draw_borrow_enabled,
            owner_group_plan_diagnostics: self.owner_group_plan_diagnostics,
            owner_group_plan_enabled: self.owner_group_plan_enabled,
            owner_group_work: Default::default(),
            pattern_textures: HashMap::new(),
            referenced_chart_owner: None,
            retained_area_triangulations: AreaTriangulationRetention::default(),
            retained_world_areas: self.retained_world_areas.fork_cold(),
            source_line_projection_arena: self.source_line_projection_arena.fork_cold(),
            spatial_hierarchy_enabled: self.spatial_hierarchy_enabled,
            dense_area_candidates_baseline: self.dense_area_candidates_baseline,
            static_line_bounds: self.static_line_bounds.fork_cold(),
            static_source_classification_enabled: self.static_source_classification_enabled,
            suppression_tail: None,
            symbol_class_cache: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            symbol_textures: HashMap::with_capacity(100),
            triangulation_cache: HashMap::with_capacity(500),
            triangulation_failures: FxHashSet::default(),
            triangulation_revision: None,
            vector_frame: VectorFrameState {
                frame_cpu: VectorGeometryCpu {
                    world_map_line_vertices: Vec::with_capacity(2000),
                    world_map_line_indices: Vec::with_capacity(6000),
                    world_map_mask_vertices: Vec::new(),
                    world_map_mask_indices: Vec::new(),
                    area_vertices: Vec::with_capacity(10000),
                    area_indices: Vec::with_capacity(30000),
                    line_geometry: crate::primary_line_geometry::Geometry::new_frame(
                        5000,
                        15000,
                        self.primary_line_quad_enabled,
                    ),
                    symbol_instances: Vec::with_capacity(2000),
                    pattern_vertices: Vec::with_capacity(5000),
                    pattern_indices: Vec::with_capacity(15000),
                    text_labels: Vec::with_capacity(500),
                    area_priority_ranges: Vec::with_capacity(10),
                    line_priority_ranges: Vec::with_capacity(10),
                    symbol_priority_ranges: Vec::with_capacity(10),
                    pattern_ranges: Vec::with_capacity(10),
                    displayed_geometry: Vec::new(),
                    pattern_emission_audit: self
                        .vector_frame
                        .frame_cpu
                        .pattern_emission_audit
                        .as_ref()
                        .map(|_| Vec::new()),
                    pattern_emission_audit_dropped: 0,
                    owner: Arc::new(()),
                },
                geometry_transform: None,
                temporal_visibility_counts: (0, 0),
                temporal_visibility_mask: Vec::new(),
                display_scale: 1,
                coverage_scale_colour: None,
                coverage_scale_colours: Default::default(),
                screen_pan_offset: (0.0, 0.0),
                selection_anchor: None,
                selection_world_geometry: Vec::new(),
                selection_screen_geometry: Vec::new(),
                selection_index: std::cell::OnceCell::new(),
                dependency_status: DependencyRenderStatus::default(),
                screen_zoom_scale: 1.0,
                screen_zoom_scale_y: 1.0,
                screen_zoom_pivot: (0.0, 0.0),
                view_dependent_symbols: false,
                view_clipped_patterns: false,
                scene_bounds: None,
                viewport_world_bounds: None,
                prepared_coverage: None,
                overscale_annotation: Vec::new(),
                coverage_frame: None,
                coverage_failed: false,
                emitting_coverage_source: None,
                device_fixed_sources: FxHashSet::default(),
                static_source_classification: None,
                chart_geometry_viewport: None,
                world_map_chart_boxes: Vec::new(),
                lon_wrap_screen_px: 0.0,
            },
        }
    }
}

/// An emitted candidate with independently owned CPU/cache/material/font state.
/// NOT a complete Ready scene: geometry/text GPU uploads and joint raster/App validation remain.
/// Retained single-PC inputs. This is resource ownership, not source authentication.
/// Construction keeps the actual captured PC owner and derives its profile; callers
/// still validate dataset/FC/PC compatibility before staging.
pub struct SinglePcVectorResources {
    cache: SymbolCache,
    profile: ColorProfile,
    catalogue: std::sync::Arc<ferrite_portrayal_catalog::BoundPortrayalCatalogue>,
    visible_groups: Option<std::collections::HashSet<u32>>,
}
impl SinglePcVectorResources {
    pub fn new(
        cache: SymbolCache,
        catalogue: std::sync::Arc<ferrite_portrayal_catalog::BoundPortrayalCatalogue>,
        palette: &str,
        visible_groups: Option<std::collections::HashSet<u32>>,
    ) -> Result<Self> {
        if !cache.has_exact_source_owner(&catalogue.sources()) {
            return Err(WgpuError::Render(
                "Single PC cache has a foreign captured source owner".into(),
            ));
        }
        if visible_groups.as_ref().is_some_and(|g| g.len() > 4096) {
            return Err(WgpuError::Render(
                "Single PC viewing-group receiver limit".into(),
            ));
        }
        let profile = catalogue
            .color_profiles
            .get_profile(palette)
            .ok_or_else(|| WgpuError::Render("Single PC palette unavailable".into()))?
            .clone();
        Ok(Self {
            cache,
            profile,
            catalogue,
            visible_groups,
        })
    }
    pub fn catalogue(&self) -> &std::sync::Arc<ferrite_portrayal_catalog::BoundPortrayalCatalogue> {
        &self.catalogue
    }
    pub fn profile(&self) -> &ColorProfile {
        &self.profile
    }
    pub fn visible_groups(&self) -> Option<&std::collections::HashSet<u32>> {
        self.visible_groups.as_ref()
    }
    pub fn into_cache(self) -> SymbolCache {
        self.cache
    }
}
enum PrivateVectorResources {
    OwnedCells(crate::CellPortrayalResources),
    SinglePc(Box<SinglePcVectorResources>),
}
impl PrivateVectorResources {
    fn is_owned_cells(&self) -> bool {
        matches!(self, Self::OwnedCells(_))
    }
    fn seal(&mut self) -> Result<()> {
        match self {
            Self::OwnedCells(resources) => resources
                .seal_viewing_groups()
                .map_err(|e| WgpuError::Render(e.to_string())),
            Self::SinglePc(single) => {
                if !single
                    .cache
                    .has_exact_source_owner(&single.catalogue.sources())
                {
                    return Err(WgpuError::Render(
                        "Single PC captured source owner changed".into(),
                    ));
                }
                Ok(())
            }
        }
    }
    fn emit(
        &mut self,
        emission: &mut VectorEmissionOwned,
        services: &mut EmissionServices<'_>,
        context: &mut RenderContext,
        owned_groups: Option<&std::collections::HashSet<u32>>,
    ) -> Result<()> {
        match self {
            Self::OwnedCells(resources) => emission.add_instructions_with_resources_impl(
                services,
                context,
                None,
                None,
                owned_groups,
                Some(resources),
            ),
            Self::SinglePc(single) => emission.add_instructions_with_resources_impl(
                services,
                context,
                Some(&mut single.cache),
                Some(&single.profile),
                single.visible_groups.as_ref(),
                None,
            ),
        }
    }
}
/// Single-PC wrapper intentionally exposes no OwnedCells-only getters or unwrap.
pub struct PreparedSinglePcVectorEmission(PreparedVectorEmission);
impl PreparedSinglePcVectorEmission {
    pub fn context(&self) -> &RenderContext {
        self.0.context()
    }
    pub fn output_counts(&self) -> [usize; 5] {
        self.0.output_counts()
    }
}

/// One private OLD renderer origin captured before App Lua/numeric staging.
/// This is an expected-old binding, not permission to publish target resources.
pub struct RendererPublicationOrigin {
    expected_old: private_vector_gpu::ExpectedPublishedVector,
    old_scene_epoch: private_vector_gpu::VectorSceneEpoch,
    environment: PrivateEmissionEnvironment,
}
pub struct PreparedVectorEmission {
    emission: VectorEmissionOwned,
    context: RenderContext,
    resources: PrivateVectorResources,
    environment: PrivateEmissionEnvironment,
    old_scene_epoch: private_vector_gpu::VectorSceneEpoch,
    // One expected-old snapshot spans CPU preparation through GPU/activation.
    // Never recapture old state after CPU work: a policy/owner change must refuse.
    expected_old: private_vector_gpu::ExpectedPublishedVector,
    settings: VectorEmissionSettings,
}
impl PreparedVectorEmission {
    pub fn context(&self) -> &RenderContext {
        &self.context
    }
    pub fn target_continuous_transform_key(&self) -> [u32; 9] {
        self.emission
            .vector_frame
            .continuous_transform_key(self.environment.viewport_size())
    }
    pub fn output_counts(&self) -> [usize; 5] {
        let cpu = &self.emission.vector_frame.frame_cpu;
        [
            cpu.area_vertices.len(),
            cpu.area_indices.len(),
            cpu.line_geometry.vertex_len(),
            cpu.line_geometry.index_len(),
            cpu.symbol_instances.len(),
        ]
    }
    pub fn has_private_chart_fonts(&self) -> bool {
        self.emission.referenced_chart_owner.is_some()
    }
    pub fn resource_owners(&self) -> &crate::CellPortrayalResources {
        match &self.resources {
            PrivateVectorResources::OwnedCells(resources) => resources,
            PrivateVectorResources::SinglePc(_) => {
                unreachable!("Only the sealed SinglePc wrapper can contain SinglePc")
            }
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct PrivateEmissionEnvironment {
    extent: [u32; 2],
    viewport: [u32; 2],
    density: u64,
    ppp: u32,
    zoom: u32,
    format: wgpu::TextureFormat,
}
impl PrivateEmissionEnvironment {
    fn capture(renderer: &WgpuRenderer) -> Self {
        let (w, h) = renderer.state.viewport_size();
        Self {
            extent: [renderer.state.size.width, renderer.state.size.height],
            viewport: [w.to_bits(), h.to_bits()],
            density: renderer.state.window.scale_factor().to_bits(),
            ppp: renderer.egui.ctx.pixels_per_point().to_bits(),
            zoom: renderer.egui.ctx.zoom_factor().to_bits(),
            format: renderer.state.format(),
        }
    }
    fn viewport_size(self) -> (f32, f32) {
        (
            f32::from_bits(self.viewport[0]),
            f32::from_bits(self.viewport[1]),
        )
    }
    fn private_metric_source(self, source: &egui::Context) -> Result<egui::Context> {
        let density = f64::from_bits(self.density) as f32;
        let ppp = f32::from_bits(self.ppp);
        let zoom = f32::from_bits(self.zoom);
        if self.extent.contains(&0)
            || !density.is_finite()
            || density <= 0.
            || !ppp.is_finite()
            || ppp <= 0.
            || !zoom.is_finite()
            || zoom <= 0.
        {
            return Err(WgpuError::Render(
                "Invalid private chart font environment".into(),
            ));
        }
        // Read options only. No Context clone, live begin_pass, live font definitions or atlas writes.
        let context = egui::Context::default();
        context.options_mut(|o| *o = source.options(Clone::clone));
        context.set_zoom_factor(zoom);
        context.set_fonts(crate::chart_fonts::chart_font_definitions());
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(
                    self.extent[0] as f32 / density,
                    self.extent[1] as f32 / density,
                ),
            )),
            max_texture_side: Some(2048),
            ..Default::default()
        };
        raw.viewports
            .entry(context.viewport_id())
            .or_default()
            .native_pixels_per_point = Some(density);
        context.begin_pass(raw);
        let actual_ppp = context.pixels_per_point().to_bits();
        let _output = context.end_pass();
        if actual_ppp != self.ppp {
            return Err(WgpuError::Render(
                "Private chart metrics differ from active physical DPI".into(),
            ));
        }
        Ok(context)
    }
}
impl WgpuRenderer {
    pub fn capture_publication_origin(&self) -> RendererPublicationOrigin {
        RendererPublicationOrigin {
            expected_old: private_vector_gpu::ExpectedPublishedVector::capture(self),
            old_scene_epoch: self.vector_scene_epoch.clone(),
            environment: PrivateEmissionEnvironment::capture(self),
        }
    }
    pub fn validate_publication_origin(&self, origin: &RendererPublicationOrigin) -> Result<()> {
        if !origin.expected_old.matches(self)
            || origin.environment != PrivateEmissionEnvironment::capture(self)
        {
            return Err(WgpuError::Render(
                "Early App publication origin changed".into(),
            ));
        }
        Ok(())
    }
    /// The App captures origin BEFORE preparing Lua/rasters/catalogue target.
    /// It must transfer that same origin here; no late expected-old recapture.
    #[allow(clippy::too_many_arguments)] // Explicit OLD origin is separate from TARGET inputs.
    pub fn prepare_private_vector_emission_from_origin(
        &self,
        origin: RendererPublicationOrigin,
        context: RenderContext,
        resources: crate::CellPortrayalResources,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
        settings: VectorEmissionSettings,
        longitude_wrap_pixels: f32,
        world_map_chart_boxes: Vec<(f64, f64, f64, f64)>,
    ) -> Result<PreparedVectorEmission> {
        self.validate_publication_origin(&origin)?;
        self.prepare_private_vector_emission_impl(
            context,
            PrivateVectorResources::OwnedCells(resources),
            visible_viewing_groups,
            settings,
            longitude_wrap_pixels,
            world_map_chart_boxes,
            Some(origin),
        )
    }
    /// Prepare the actual shared dependency/coverage/point/line/area/text emitter on a new owner.
    /// Caller transfers a candidate context and candidate sealed PC resources, never the published inputs.
    /// This API does not activate, swap, or mutate the published scene and does not mint raster proof.
    pub fn prepare_private_vector_emission(
        &self,
        context: RenderContext,
        resources: crate::CellPortrayalResources,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
        settings: VectorEmissionSettings,
        longitude_wrap_pixels: f32,
        world_map_chart_boxes: Vec<(f64, f64, f64, f64)>,
    ) -> Result<PreparedVectorEmission> {
        self.prepare_private_vector_emission_impl(
            context,
            PrivateVectorResources::OwnedCells(resources),
            visible_viewing_groups,
            settings,
            longitude_wrap_pixels,
            world_map_chart_boxes,
            Some(self.capture_publication_origin()),
        )
    }
    /// Single-PC counterpart: retain the same early OLD binding across preparation.
    pub fn prepare_private_single_pc_vector_emission_from_origin(
        &self,
        origin: RendererPublicationOrigin,
        context: RenderContext,
        resources: SinglePcVectorResources,
        settings: VectorEmissionSettings,
        longitude_wrap_pixels: f32,
        world_map_chart_boxes: Vec<(f64, f64, f64, f64)>,
    ) -> Result<PreparedSinglePcVectorEmission> {
        self.validate_publication_origin(&origin)?;
        self.prepare_private_vector_emission_impl(
            context,
            PrivateVectorResources::SinglePc(Box::new(resources)),
            None,
            settings,
            longitude_wrap_pixels,
            world_map_chart_boxes,
            Some(origin),
        )
        .map(PreparedSinglePcVectorEmission)
    }
    /// Original global symbols/profile overload, with captured SinglePc inputs.
    /// Ownerless instructions stay ownerless; no synthetic cell owner is inserted.
    pub fn prepare_private_single_pc_vector_emission(
        &self,
        context: RenderContext,
        resources: SinglePcVectorResources,
        settings: VectorEmissionSettings,
        longitude_wrap_pixels: f32,
        world_map_chart_boxes: Vec<(f64, f64, f64, f64)>,
    ) -> Result<PreparedSinglePcVectorEmission> {
        self.prepare_private_vector_emission_impl(
            context,
            PrivateVectorResources::SinglePc(Box::new(resources)),
            None,
            settings,
            longitude_wrap_pixels,
            world_map_chart_boxes,
            Some(self.capture_publication_origin()),
        )
        .map(PreparedSinglePcVectorEmission)
    }
    #[allow(clippy::too_many_arguments)] // Shared emitter keeps origin and target policy distinct.
    fn prepare_private_vector_emission_impl(
        &self,
        mut context: RenderContext,
        mut resources: PrivateVectorResources,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
        settings: VectorEmissionSettings,
        longitude_wrap_pixels: f32,
        world_map_chart_boxes: Vec<(f64, f64, f64, f64)>,
        origin: Option<RendererPublicationOrigin>,
    ) -> Result<PreparedVectorEmission> {
        if let Some(origin) = &origin {
            self.validate_publication_origin(origin)?;
        }
        if !settings.symbol_scale.is_finite() || settings.symbol_scale <= 0. {
            return Err(WgpuError::Render(
                "Invalid private vector symbol scale".into(),
            ));
        }
        if settings
            .background_color
            .to_array()
            .iter()
            .any(|v| !v.is_finite())
        {
            return Err(WgpuError::Render(
                "Invalid private vector background colour".into(),
            ));
        }
        if !longitude_wrap_pixels.is_finite() || longitude_wrap_pixels < 0. {
            return Err(WgpuError::Render(
                "Invalid private vector longitude wrap".into(),
            ));
        }
        resources.seal()?;
        let RendererPublicationOrigin {
            expected_old,
            old_scene_epoch,
            environment,
        } = origin.unwrap_or_else(|| self.capture_publication_origin());
        let metrics = environment.private_metric_source(&self.egui.ctx)?;
        let mut emission = self.vector_emission.fork_private();
        emission.vector_frame.lon_wrap_screen_px = longitude_wrap_pixels;
        emission.vector_frame.world_map_chart_boxes = world_map_chart_boxes;
        // No UI state borrow, live fonts, old writable GPU views, glyph pools, atlas or buffer clones.
        let mut services = EmissionServices {
            state: &self.state,
            pipelines: &self.pipelines,
            fonts: EmissionFonts::Private(&metrics),
            cpu_profiler: None,
            overscale_program_reuse: &self.overscale_program_reuse,
            symbol_scale: settings.symbol_scale,
            show_soundings: settings.show_soundings,
            animation_mode: settings.animation_mode,
            show_shallow_pattern: settings.show_shallow_pattern,
            world_map_coastlines: &self.world_map_coastlines,
            world_map_detailed: &self.world_map_detailed,
            background_color: settings.background_color,
        };
        emission.update_viewport_bounds(&mut services, &context.scaler);
        emission.add_world_map_lines(&mut services, &context.scaler);
        resources.emit(
            &mut emission,
            &mut services,
            &mut context,
            visible_viewing_groups,
        )?;
        if environment != PrivateEmissionEnvironment::capture(self) || !expected_old.matches(self) {
            return Err(WgpuError::Render(
                "Private vector preparation environment changed".into(),
            ));
        }
        Ok(PreparedVectorEmission {
            emission,
            context,
            resources,
            environment,
            old_scene_epoch,
            expected_old,
            settings,
        })
    }
}

#[cfg(test)]
mod private_vector_emitter_ownership_tests {
    use super::*;
    fn fixture() -> VectorEmissionOwned {
        VectorEmissionOwned {
            scene_draw_plan: vector_scene_draw::scene_draw_plan::Cache::new(None),
            primary_line_quad_enabled: false,
            accepted_screen_line_packet: crate::accepted_screen_line_packet::Capture::new(None),
            area_projection_shadow: crate::retained_world_area::ExactAreaShadowCache::new(false),
            area_triangulation_reuse_enabled: false,
            cached_symbol_buffers: Vec::new(),
            compact_owner_admission_diagnostics: false,
            compact_owner_admission_enabled: false,
            current_frame_admission_reuse_enabled: false,
            compact_owner_work: Default::default(),
            coverage_pipelines: None,
            coverage_trial_reuse: false,
            coverage_trial_work: Default::default(),
            emitter_wave_work: None,
            empty_symbol_ref_count: 0,
            flat_diagnostic: None,
            flat_gpu_coverage_host_ns: 0,
            flat_stage_timing_enabled: false,
            frame_local_coverage_clip_reuse: false,
            gpu_buffers_dirty: true,
            gpu_timestamp_requested: false,
            gpu_timestamp_target_camera: None,
            line_preparation_work: None,
            line_suppression: ferrite_render::LineSuppressionCache::default(),
            missing_symbol_ids: FxHashSet::with_capacity_and_hasher(32, Default::default()),
            moving_line_northing: crate::moving_line_northing::Cache::new(None),
            source_batch_parallel: crate::source_batch_parallel::Cache::new(None),
            native_route_target_camera: None,
            owner_draw_borrow_enabled: false,
            owner_group_plan_diagnostics: false,
            owner_group_plan_enabled: false,
            owner_group_work: Default::default(),
            pattern_textures: HashMap::new(),
            referenced_chart_owner: None,
            retained_area_triangulations: AreaTriangulationRetention::default(),
            retained_world_areas: crate::retained_world_area::RetainedWorldAreas::new(None),
            source_line_projection_arena: crate::source_line_projection_arena::Cache::new(None),
            spatial_hierarchy_enabled: false,
            dense_area_candidates_baseline: false,
            static_line_bounds: crate::static_line_bounds::Cache::new(None),
            static_source_classification_enabled: false,
            suppression_tail: None,
            symbol_class_cache: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            symbol_textures: HashMap::with_capacity(100),
            triangulation_cache: HashMap::with_capacity(500),
            triangulation_failures: FxHashSet::default(),
            triangulation_revision: None,
            vector_frame: VectorFrameState::empty_fixture(),
        }
    }
    #[test]
    fn private_primary_fork_starts_own_empty_packed_geometry_and_logical_counts() {
        let mut displayed = fixture();
        displayed.primary_line_quad_enabled = true;
        displayed
            .vector_frame
            .frame_cpu
            .line_geometry
            .begin_frame(true);
        displayed
            .vector_frame
            .frame_cpu
            .line_geometry
            .append_emitted([0., -0.], [5., 10.], [-0., -1.], [0., 1.], [1.; 4])
            .unwrap();
        let prefix = displayed.vector_frame.frame_cpu.prefix();
        let mut private = displayed.fork_private();
        assert!(private.primary_line_quad_enabled);
        assert!(private
            .vector_frame
            .frame_cpu
            .line_geometry
            .packed()
            .is_some());
        assert_eq!(
            (
                private.vector_frame.frame_cpu.line_geometry.vertex_len(),
                private.vector_frame.frame_cpu.line_geometry.index_len()
            ),
            (0, 0)
        );
        assert!(private
            .vector_frame
            .frame_cpu
            .truncate_trial(&prefix)
            .is_err());
        assert_eq!(
            displayed.vector_frame.frame_cpu.line_geometry.index_len(),
            6
        );
        private
            .vector_frame
            .frame_cpu
            .line_geometry
            .append_emitted([0., -0.], [5., 10.], [-0., -1.], [0., 1.], [1.; 4])
            .unwrap();
        let (a, ai) = displayed
            .vector_frame
            .frame_cpu
            .line_geometry
            .export_legacy()
            .unwrap();
        let (b, bi) = private
            .vector_frame
            .frame_cpu
            .line_geometry
            .export_legacy()
            .unwrap();
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&a),
            bytemuck::cast_slice::<_, u8>(&b)
        );
        assert_eq!(ai, bi);
    }
    #[test]
    fn private_fork_keeps_policy_without_aliasing_current_geometry_or_font_owner() {
        let mut displayed = fixture();
        displayed.frame_local_coverage_clip_reuse = true;
        displayed.coverage_trial_reuse = true;
        displayed.static_source_classification_enabled = true;
        displayed
            .vector_frame
            .frame_cpu
            .area_indices
            .extend([0, 1, 2]);
        displayed
            .vector_frame
            .frame_cpu
            .world_map_line_indices
            .extend([0, 1]);
        displayed.vector_frame.lon_wrap_screen_px = 111.;
        let old_prefix = displayed.vector_frame.frame_cpu.prefix();
        let mut candidate = displayed.fork_private();
        assert!(!Arc::ptr_eq(
            &displayed.vector_frame.frame_cpu.owner,
            &candidate.vector_frame.frame_cpu.owner
        ));
        assert!(
            candidate.frame_local_coverage_clip_reuse
                && candidate.coverage_trial_reuse
                && candidate.static_source_classification_enabled
        );
        assert!(candidate.vector_frame.frame_cpu.area_indices.is_empty());
        assert!(candidate
            .vector_frame
            .frame_cpu
            .world_map_line_indices
            .is_empty());
        assert!(
            candidate.referenced_chart_owner.is_none()
                && candidate.cached_symbol_buffers.is_empty()
        );
        assert!(
            candidate.vector_frame.coverage_frame.is_none()
                && candidate.vector_frame.overscale_annotation.is_empty()
        );
        assert!(candidate
            .vector_frame
            .frame_cpu
            .truncate_trial(&old_prefix)
            .is_err());
        candidate.vector_frame.frame_cpu.area_indices.push(99);
        assert_eq!(displayed.vector_frame.frame_cpu.area_indices, [0, 1, 2]);
        assert_eq!(
            displayed.vector_frame.frame_cpu.world_map_line_indices,
            [0, 1]
        );
        assert_eq!(displayed.vector_frame.lon_wrap_screen_px, 111.);
    }
    #[test]
    fn private_font_source_covers_no_reference_labels_and_is_not_ui_source() {
        let source = egui::Context::default();
        source.set_fonts(crate::chart_fonts::chart_font_definitions());
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400., 300.),
            )),
            ..Default::default()
        };
        raw.viewports
            .entry(source.viewport_id())
            .or_default()
            .native_pixels_per_point = Some(1.);
        source.begin_pass(raw);
        let _ = source.end_pass();
        let env = PrivateEmissionEnvironment {
            extent: [400, 300],
            viewport: [400f32.to_bits(), 300f32.to_bits()],
            density: 1f64.to_bits(),
            ppp: source.pixels_per_point().to_bits(),
            zoom: source.zoom_factor().to_bits(),
            format: wgpu::TextureFormat::Rgba8Unorm,
        };
        let private = env.private_metric_source(&source).unwrap();
        let fonts = EmissionFonts::Private(&private);
        assert!(fonts.is_private());
        assert!(!EmissionFonts::ReadOnly(&source).is_private());
        assert_eq!(
            fonts.context().pixels_per_point().to_bits(),
            source.pixels_per_point().to_bits()
        );
        // Same actual built-in definitions; no owner/reference inventory needed for admission.
        crate::referenced_chart_owner::validate_all_text([("Bundled-only text", 12.)]).unwrap();
        assert!(crate::referenced_chart_owner::validate_all_text([("bad", f32::NAN)]).is_err());
    }
}

/// Explicit candidate display policy: never stage by temporarily mutating live UI settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorEmissionSettings {
    pub symbol_scale: f32,
    pub show_soundings: bool,
    pub animation_mode: bool,
    pub show_shallow_pattern: bool,
    pub background_color: Color,
}
impl WgpuRenderer {
    pub fn vector_emission_settings(&self) -> VectorEmissionSettings {
        VectorEmissionSettings {
            symbol_scale: self.symbol_scale,
            background_color: self.background_color,
            show_soundings: self.show_soundings,
            animation_mode: self.animation_mode,
            show_shallow_pattern: self.ui_state.settings.show_shallow_pattern,
        }
    }
}
impl PreparedVectorEmission {
    pub fn settings(&self) -> VectorEmissionSettings {
        self.settings
    }
}

// Private staging exposes types through existing `pub use renderer::*`; no lib/module overwrite.
#[path = "private_vector_gpu.rs"]
mod private_vector_gpu;
pub use private_vector_gpu::{
    ActivatedSinglePcVectorBindings, ActivatedVectorBindings, PreparedJointVectorRaster,
    PreparedTargetSinglePcVectorRaster, PreparedTargetVectorRaster, PrivateVectorReadback,
    ReadySinglePcVectorGpuFrame, ReadyVectorGpuFrame,
};

impl VectorEmissionOwned {
    fn pack_symbols(
        &self,
        services: &mut EmissionServices<'_>,
        start: usize,
        end: usize,
        vertices: &mut Vec<TextureVertex>,
        indices: &mut Vec<u32>,
        ranges: &mut Vec<((u64, SymbolId), u32, u32)>,
    ) {
        vertices.clear();
        indices.clear();
        ranges.clear();

        // Pack in portrayal order; merge only consecutive equal textures.
        for instance in &self.vector_frame.frame_cpu.symbol_instances[start..end] {
            if let Some(quad) = self.symbol_quad(services, instance) {
                let vertex_offset = vertices.len() as u32;
                let index_start = indices.len() as u32;
                vertices.extend_from_slice(&quad);
                indices.extend_from_slice(&[
                    vertex_offset,
                    vertex_offset + 1,
                    vertex_offset + 2,
                    vertex_offset,
                    vertex_offset + 2,
                    vertex_offset + 3,
                ]);
                if let Some((id, start, count)) = ranges.last_mut() {
                    if *id == (instance.resource_owner, instance.symbol_id)
                        && *start + *count == index_start
                    {
                        *count += 6;
                        continue;
                    }
                }
                ranges.push((
                    (instance.resource_owner, instance.symbol_id),
                    index_start,
                    6,
                ));
            }
        }
    }
}

impl VectorFrameState {
    fn view_uniforms(&self, (width, height): (f32, f32)) -> [Option<ViewUniforms>; 3] {
        let mut center = ViewUniforms::with_pan_zoom(
            width,
            height,
            1.0,
            self.screen_pan_offset.0,
            self.screen_pan_offset.1,
            self.screen_zoom_scale,
            self.screen_zoom_pivot.0,
            self.screen_zoom_pivot.1,
        );
        center.zoom_scale_y = self.screen_zoom_scale_y;
        let mut output = [Some(center), None, None];
        if self.lon_wrap_screen_px > 0. {
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
            output[1] = Some(left);
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
            output[2] = Some(right);
        }
        output
    }
}
fn chart_text_scissor(clip: egui::Rect, ppp: f32, [width, height]: [u32; 2]) -> Option<[u32; 4]> {
    let x = (clip.min.x * ppp).round().clamp(0., width as f32) as u32;
    let y = (clip.min.y * ppp).round().clamp(0., height as f32) as u32;
    let right = (clip.max.x * ppp).round().clamp(x as f32, width as f32) as u32;
    let bottom = (clip.max.y * ppp).round().clamp(y as f32, height as f32) as u32;
    (x != right && y != bottom).then_some([x, y, right - x, bottom - y])
}
fn chart_text_vertices(mesh: &egui::epaint::Mesh, ppp: f32) -> Vec<crate::ChartTextVertex> {
    mesh.vertices
        .iter()
        .map(|v| crate::ChartTextVertex {
            position: [v.pos.x * ppp, v.pos.y * ppp],
            uv: [v.uv.x, v.uv.y],
            color: v.color.to_array(),
        })
        .collect()
}

#[cfg(test)]
mod private_gpu_shared_math_tests {
    use super::*;
    #[test]
    fn scissor_and_glyph_vertices_keep_original_rounding_and_colour_bytes() {
        let clip = egui::Rect::from_min_max(egui::pos2(-2.5, 3.25), egui::pos2(19.75, 1000.));
        assert_eq!(
            chart_text_scissor(clip, 2., [31, 100]),
            Some([0, 7, 31, 93])
        );
        assert!(chart_text_scissor(
            egui::Rect::from_min_max(egui::pos2(40., 0.), egui::pos2(50., 1.)),
            1.0,
            [31, 100]
        )
        .is_none());
        let mesh = egui::epaint::Mesh {
            vertices: vec![egui::epaint::Vertex {
                pos: egui::pos2(1.25, -3.5),
                uv: egui::pos2(0.1, 0.9),
                color: egui::Color32::from_rgba_premultiplied(1, 2, 3, 4),
            }],
            ..Default::default()
        };
        let v = chart_text_vertices(&mesh, 1.5);
        assert_eq!(v[0].position, [1.875, -5.25]);
        assert_eq!(v[0].uv, [0.1, 0.9]);
        assert_eq!(v[0].color, [1, 2, 3, 4]);
    }
}

#[path = "vector_scene_draw.rs"]
mod vector_scene_draw;
impl WgpuRenderer {
    /// Opt-in upload-only counters; permission/emission and CPU indices remain original.
    pub fn area_index_upload_reuse_enabled(&self) -> bool {
        self.area_index_upload_reuse.enabled()
    }
    pub fn area_index_upload_reuse_max_metadata_bytes(&self) -> usize {
        self.area_index_upload_reuse.max_metadata_bytes()
    }
    pub fn area_index_upload_reuse_work(&self) -> &crate::AreaIndexUploadReuseWork {
        self.area_index_upload_reuse.work()
    }
    fn displayed_vector_draw_scene(&self) -> vector_scene_draw::VectorDrawScene<'_> {
        use vector_scene_draw::{PairRef, VectorDrawScene};
        VectorDrawScene {
            emission: &self.vector_emission,
            pipelines: &self.pipelines,
            area: PairRef {
                vertices: &self.cached_area_vb,
                indices: &self.cached_area_ib,
            },
            line: PairRef {
                vertices: &self.cached_line_vb,
                indices: &self.cached_line_ib,
            },
            pattern: PairRef {
                vertices: &self.cached_pattern_vb,
                indices: &self.cached_pattern_ib,
            },
            world_lines: PairRef {
                vertices: &self.cached_wm_line_vb,
                indices: &self.cached_wm_line_ib,
            },
            world_masks: PairRef {
                vertices: &self.cached_wm_mask_vb,
                indices: &self.cached_wm_mask_ib,
            },
            views: [
                &self.view_bind_group,
                &self.view_bind_group_left,
                &self.view_bind_group_right,
            ],
            line_compact: self.cached_line_compact,
            compact: self.exact_line_quad_pipelines.as_ref(),
            symbols: &self.vector_emission.cached_symbol_buffers,
            text: &self.chart_text_meshes,
            extent: [self.state.size.width, self.state.size.height],
            background: self.background_color,
            draw_range_index_enabled: self.draw_range_index_enabled,
            raster_renderer: Some(self),
        }
    }
}

impl WgpuRenderer {
    /// Immutable current chart-box inputs for a one-shot independently owned rebuild audit.
    /// No GPU resources or permission decisions are exposed.
    pub fn vector_world_map_chart_boxes(&self) -> &[(f64, f64, f64, f64)] {
        &self.vector_emission.vector_frame.world_map_chart_boxes
    }
}

#[cfg(test)]
mod single_pc_private_resource_controls {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ferrite-single-private-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("portrayal_catalogue.xml"), "<portrayalCatalog><foundationMode/><displayPlanes><displayPlane id='OverRadar' order='1'/></displayPlanes></portrayalCatalog>").unwrap();
            std::fs::create_dir(path.join("ColorProfiles")).unwrap();
            std::fs::write(
                path.join("ColorProfiles/colorProfile.xml"),
                "<colorProfile><palette name='Day'></palette></colorProfile>",
            )
            .unwrap();
            Self(path)
        }
        fn pc(&self) -> Arc<ferrite_portrayal_catalog::BoundPortrayalCatalogue> {
            Arc::new(ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&self.0).unwrap())
        }
        fn cache(&self, pc: &ferrite_portrayal_catalog::BoundPortrayalCatalogue) -> SymbolCache {
            SymbolCache::new_with_sources(self.0.join("Symbols"), pc.sources())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn identical_bytes_new_capture_is_foreign_but_same_owner_clone_is_valid() {
        let f = Fixture::new();
        let pc = f.pc();
        let foreign = f.pc();
        assert_eq!(pc.source_digest(), foreign.source_digest());
        assert!(SinglePcVectorResources::new(f.cache(&pc), foreign, "Day", None).is_err());
        assert!(SinglePcVectorResources::new(f.cache(&pc), Arc::clone(&pc), "Day", None).is_ok());
        assert!(SinglePcVectorResources::new(SymbolCache::new(&f.0), pc, "Day", None).is_err());
    }
    #[test]
    fn captured_groups_preserve_none_empty_and_values_without_a_cell_binding() {
        let f = Fixture::new();
        let pc = f.pc();
        let none = SinglePcVectorResources::new(f.cache(&pc), pc.clone(), "Day", None).unwrap();
        assert!(none.visible_groups().is_none());
        let empty =
            SinglePcVectorResources::new(f.cache(&pc), pc.clone(), "Day", Some(Default::default()))
                .unwrap();
        assert!(empty.visible_groups().unwrap().is_empty());
        let groups = std::collections::HashSet::from([21010, 33010]);
        let single =
            SinglePcVectorResources::new(f.cache(&pc), pc.clone(), "Day", Some(groups.clone()))
                .unwrap();
        assert_eq!(single.visible_groups(), Some(&groups));
        assert!(!PrivateVectorResources::SinglePc(Box::new(single)).is_owned_cells());
    }
    #[test]
    fn missing_palette_and_overbudget_groups_decline_before_emission() {
        let f = Fixture::new();
        let pc = f.pc();
        assert!(SinglePcVectorResources::new(f.cache(&pc), pc.clone(), "Missing", None).is_err());
        let groups = (0..4097).collect();
        assert!(SinglePcVectorResources::new(f.cache(&pc), pc, "Day", Some(groups)).is_err());
    }
}
// ===== Background scene build (navigation) =====

/// Inputs for one scene emission on a worker thread. Owns a warm emission
/// builder, a source fork of the context and the cell resources; nothing in it
/// is shared mutably with the UI thread.
pub struct SceneBuildJob {
    emission: VectorEmissionOwned,
    context: RenderContext,
    resources: crate::CellPortrayalResources,
    visible_viewing_groups: Option<std::collections::HashSet<u32>>,
    gpu: GpuState,
    pipelines: RenderPipelines,
    programs: Arc<crate::overscale_annotation::ProgramReuse>,
    coastlines: Arc<ferrite_render::BackgroundCoastlines>,
    coastlines_detailed: Arc<ferrite_render::BackgroundCoastlines>,
    fonts: egui::Context,
    settings: VectorEmissionSettings,
    exact_line_quad: bool,
    generation: u64,
}

/// A finished background emission waiting for `install_built_scene`.
pub struct BuiltScene {
    emission: VectorEmissionOwned,
    context: RenderContext,
    resources: crate::CellPortrayalResources,
    settings: VectorEmissionSettings,
    generation: u64,
    prebuilt: Vec<crate::state::PrebuiltBuffer>,
    result: std::result::Result<(), String>,
    pub build_time: std::time::Duration,
}

/// Ownership returned to the App whether or not the scene was installed.
pub struct InstalledScene {
    /// The built context (its camera and prepared coverage), when installed.
    pub context: Option<RenderContext>,
    pub resources: crate::CellPortrayalResources,
    pub installed: bool,
    pub reason: Option<String>,
}

impl SceneBuildJob {
    /// The source fork, for App-side preparation (coverage) before `run`.
    pub fn context_mut(&mut self) -> &mut RenderContext {
        &mut self.context
    }
    /// App-side preparation failed; return ownership without emitting.
    pub fn fail(self, reason: String) -> BuiltScene {
        BuiltScene {
            emission: self.emission,
            context: self.context,
            resources: self.resources,
            settings: self.settings,
            generation: self.generation,
            prebuilt: Vec::new(),
            result: Err(reason),
            build_time: std::time::Duration::ZERO,
        }
    }
    /// Run on any thread. Mirrors the UI-thread rebuild order: reset, world
    /// map, then the shared cell emitter.
    pub fn run(mut self) -> BuiltScene {
        let start = std::time::Instant::now();
        self.emission.begin_frame(self.exact_line_quad);
        let result = {
            let mut services = EmissionServices {
                state: &self.gpu,
                pipelines: &self.pipelines,
                fonts: EmissionFonts::Private(&self.fonts),
                cpu_profiler: None,
                overscale_program_reuse: &self.programs,
                symbol_scale: self.settings.symbol_scale,
                show_soundings: self.settings.show_soundings,
                animation_mode: self.settings.animation_mode,
                show_shallow_pattern: self.settings.show_shallow_pattern,
                world_map_coastlines: &self.coastlines,
                world_map_detailed: &self.coastlines_detailed,
                background_color: self.settings.background_color,
            };
            let scaler = self.context.scaler.clone();
            self.emission.add_world_map_lines(&mut services, &scaler);
            self.resources
                .seal_viewing_groups()
                .and_then(|()| {
                    self.emission.add_instructions_with_resources_impl(
                        &mut services,
                        &mut self.context,
                        None,
                        None,
                        self.visible_viewing_groups.as_ref(),
                        Some(&mut self.resources),
                    )
                })
                .map_err(|error| error.to_string())
        };
        if result.is_ok() {
            self.emission.prebuild_scene_buffers(&self.gpu);
        }
        BuiltScene {
            emission: self.emission,
            context: self.context,
            resources: self.resources,
            settings: self.settings,
            generation: self.generation,
            prebuilt: self.gpu.take_prebuilt_buffers(),
            result,
            build_time: start.elapsed(),
        }
    }
}

impl BuiltScene {
    /// Source fork the scene was emitted from (camera, prepared coverage).
    pub fn context(&self) -> &RenderContext {
        &self.context
    }
}

impl WgpuRenderer {
    fn invalidate_scene_builder(&mut self) {
        self.scene_generation = self.scene_generation.wrapping_add(1);
        self.idle_builder = None;
    }

    /// Background builds exclude raster layers, whose continuous frame is
    /// bound to the displayed camera on the UI thread.
    pub fn background_scene_build_supported(&self) -> bool {
        self.raster_layers.is_empty()
            && self.continuous_layer_count == 0
            && self.continuous_frame.is_none()
    }

    pub fn scene_build_pending(&self) -> bool {
        self.scene_build_pending
    }

    /// The visible preview is past half its drift budget: build ahead now.
    pub fn preview_refresh_due(&self) -> bool {
        self.preview_refresh_due
    }

    /// A background build can be shown through the affine while it runs.
    pub fn background_build_previewable(&self) -> bool {
        self.motion_preview && !self.requires_exact_rebuild_for_navigation()
    }

    /// UI-thread preparation before `begin_scene_build`: the independent font
    /// metrics for the current density/extent. False means build on the UI
    /// thread instead.
    pub fn prepare_background_scene_build(&mut self) -> bool {
        self.state.sync_scale_factor();
        let environment = PrivateEmissionEnvironment::capture(self);
        if self
            .worker_fonts
            .as_ref()
            .is_some_and(|(cached, _)| *cached == environment)
        {
            return true;
        }
        match environment.private_metric_source(&self.egui.ctx) {
            Ok(ctx) => {
                self.worker_fonts = Some((environment, ctx));
                true
            }
            Err(error) => {
                tracing::debug!("Background chart fonts unavailable: {error}");
                self.worker_fonts = None;
                false
            }
        }
    }

    /// Capture a background build of `context` (a `fork_for_emission` at the
    /// target camera). The caller transfers `resources` until install.
    pub fn begin_scene_build(
        &mut self,
        context: RenderContext,
        resources: crate::CellPortrayalResources,
        visible_viewing_groups: Option<std::collections::HashSet<u32>>,
    ) -> SceneBuildJob {
        // Window density is read here, on the UI thread: macOS window calls
        // from the worker would block on this thread.
        self.state.sync_scale_factor();
        let fonts = self
            .worker_fonts
            .as_ref()
            .map(|(_, ctx)| ctx.clone())
            .expect("prepare_background_scene_build checked fonts");
        let mut emission = self
            .idle_builder
            .take()
            .unwrap_or_else(|| self.vector_emission.fork_private());
        let displayed = &mut self.vector_emission;
        if emission.triangulation_revision != displayed.triangulation_revision {
            // Share immutable triangulations instead of re-triangulating cold.
            let cache = std::mem::take(&mut displayed.triangulation_cache);
            displayed.triangulation_cache = cache
                .into_iter()
                .map(|(key, storage)| match storage {
                    TriangulationStorage::Owned(owned) => {
                        (key, TriangulationStorage::Shared(Arc::new(owned)))
                    }
                    shared => (key, shared),
                })
                .collect();
            emission.triangulation_cache = displayed
                .triangulation_cache
                .iter()
                .map(|(key, storage)| match storage {
                    TriangulationStorage::Shared(v) => {
                        (*key, TriangulationStorage::Shared(Arc::clone(v)))
                    }
                    TriangulationStorage::Owned(_) => unreachable!("converted above"),
                })
                .collect();
            emission.triangulation_failures = displayed.triangulation_failures.clone();
            emission.triangulation_revision = displayed.triangulation_revision;
        }
        emission.spatial_hierarchy_enabled = displayed.spatial_hierarchy_enabled;
        emission.vector_frame.world_map_chart_boxes =
            displayed.vector_frame.world_map_chart_boxes.clone();
        emission.vector_frame.lon_wrap_screen_px = 360.0 * context.scaler.scale_x() as f32;
        self.scene_build_pending = true;
        SceneBuildJob {
            emission,
            context,
            resources,
            visible_viewing_groups,
            gpu: self.state.worker_shadow(),
            pipelines: self.pipelines.clone(),
            programs: Arc::clone(&self.overscale_program_reuse),
            coastlines: Arc::clone(&self.world_map_coastlines),
            coastlines_detailed: Arc::clone(&self.world_map_detailed),
            fonts,
            settings: self.vector_emission_settings(),
            exact_line_quad: self.exact_line_quad_enabled,
            generation: self.scene_generation,
        }
    }

    /// Swap in a finished scene if nothing it depended on changed. `current`
    /// is the App context now (sharing the built source proves no source edit).
    /// After installing, the caller re-applies the current camera affine.
    pub fn install_built_scene(
        &mut self,
        built: BuiltScene,
        current: &RenderContext,
    ) -> InstalledScene {
        self.scene_build_pending = false;
        let BuiltScene {
            mut emission,
            context,
            resources,
            settings,
            generation,
            prebuilt,
            result,
            ..
        } = built;
        let reject = if let Err(error) = result {
            Some(format!("background emission failed: {error}"))
        } else if generation != self.scene_generation {
            Some("renderer inputs changed during build".into())
        } else if settings != self.vector_emission_settings() {
            Some("display settings changed during build".into())
        } else if !context.shares_instructions_with(current)
            || context.geometry_revision() != current.geometry_revision()
        {
            Some("chart source changed during build".into())
        } else {
            None
        };
        if let Some(reason) = reject {
            if generation == self.scene_generation {
                self.idle_builder = Some(emission);
            }
            return InstalledScene {
                context: None,
                resources,
                installed: false,
                reason: Some(reason),
            };
        }
        // UI-thread owned state and diagnostics collectors follow the displayed
        // owner. Background emission work is not attributed to frame rows.
        let old = &mut self.vector_emission;
        emission.native_route_target_camera = old.native_route_target_camera.take();
        emission.flat_diagnostic = old.flat_diagnostic.take();
        emission.emitter_wave_work = old.emitter_wave_work.take();
        emission.line_preparation_work = old.line_preparation_work.take();
        emission.suppression_tail = old.suppression_tail.take();
        emission.gpu_timestamp_requested = old.gpu_timestamp_requested;
        emission.gpu_timestamp_target_camera = old.gpu_timestamp_target_camera.take();
        let frame = &mut emission.vector_frame;
        let old_frame = &mut old.vector_frame;
        frame.selection_world_geometry = std::mem::take(&mut old_frame.selection_world_geometry);
        frame.selection_screen_geometry = std::mem::take(&mut old_frame.selection_screen_geometry);
        frame.selection_anchor = old_frame.selection_anchor.take();
        frame.screen_pan_offset = (0.0, 0.0);
        frame.screen_zoom_scale = 1.0;
        frame.screen_zoom_scale_y = 1.0;
        frame.screen_zoom_pivot = (0.0, 0.0);
        std::mem::swap(&mut self.vector_emission, &mut emission);
        self.idle_builder = Some(emission);
        // The swap moved the CPU vectors without reallocating them.
        self.state.hold_prebuilt_buffers(prebuilt);
        self.vector_scene_epoch.advance();
        self.motion_preview_active = false;
        self.update_view_uniforms();
        self.update_coverage_transform();
        self.update_selection(&context.scaler);
        InstalledScene {
            context: Some(context),
            resources,
            installed: true,
            reason: None,
        }
    }
}

#[cfg(test)]
mod emission_send_contract {
    fn send<T: Send>() {}
    /// A prepared emission and its inputs may move to a worker thread.
    #[test]
    fn prepared_emission_and_inputs_are_send() {
        send::<super::VectorEmissionOwned>();
        send::<super::PreparedVectorEmission>();
        send::<ferrite_render::RenderContext>();
        send::<crate::CellPortrayalResources>();
    }
}
#[cfg(test)]
mod worker_input_send_contract {
    fn send<T: Send>() {}
    fn sync<T: Sync>() {}
    #[test]
    fn worker_inputs_are_send() {
        send::<crate::GpuState>();
        send::<super::RenderPipelines>();
        sync::<crate::overscale_annotation::ProgramReuse>();
        send::<egui::Context>();
    }
}
