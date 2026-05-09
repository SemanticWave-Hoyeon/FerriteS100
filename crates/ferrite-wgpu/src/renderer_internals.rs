//! Renderer-internal helper types and pure helper functions.
//!
//! These were originally inlined in `renderer.rs`, which had grown past
//! 3700 lines. They are intentionally `pub(super)`/`pub(crate)`-scoped:
//! consumers outside the wgpu crate should keep using `WgpuRenderer`'s
//! public surface and never touch these directly.
//!
//! Split organisation:
//! - **Symbol classification** (`SYM_*` flags + `classify_symbol`): used by
//!   the symbol-rendering hot path to avoid repeated `starts_with`.
//! - **Geometry helpers** (`point_in_ring`, `hash_geometry`): used by
//!   hit-testing and triangulation cache keys.
//! - **Spatial index** (`QuadTreeNode`): viewport-culling bookkeeping.
//! - **Render-pipeline state** (`CachedTriangulation`, `SymbolBatch`,
//!   `PatternTexture`, `SymbolTexture`, `SymbolInstance`, `TextLabel`,
//!   `TextCollisionGrid`): wgpu-side bookkeeping that backs `WgpuRenderer`'s
//!   private fields.

use rustc_hash::FxHashSet;
use std::hash::{Hash, Hasher};

use ferrite_render::WorldPoint;

use crate::pipeline::TextureVertex;

// === Symbol Classification Flags ===
// Cached per SymbolId to avoid repeated starts_with() string matching in hot paths.
// Computed once per unique symbol, reused across all frames.
pub(crate) const SYM_SOUNDING: u8 = 1 << 0; // starts_with("SOUND")
pub(crate) const SYM_NAV_AID: u8 = 1 << 1; // LIGHTS, BUOY, BCN, TOPMAR
pub(crate) const SYM_SAFETY: u8 = 1 << 2; // ISODGR, DANGER, WRECKS, OBSTRN, UWTROC, FOULAR

/// Classify a symbol string into bitflags (called once per unique symbol)
#[inline]
pub(crate) fn classify_symbol(s: &str) -> u8 {
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
pub(crate) fn point_in_ring(px: f32, py: f32, ring: &[(f32, f32)]) -> bool {
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
pub(crate) struct CachedTriangulation {
    /// Earcut triangle indices (into world_vertices)
    pub(crate) indices: Vec<usize>,
    /// Cleaned world-coordinate vertices [x0, y0, x1, y1, ...] (exterior + holes)
    pub(crate) world_vertices: Vec<f64>,
    /// World-coordinate axis-aligned bounding box (min_x, min_y, max_x, max_y)
    /// Used for O(1) viewport frustum culling — skip entire area if AABB is off-screen
    pub(crate) world_aabb: (f64, f64, f64, f64),
}

/// Batched symbols grouped by texture for efficient rendering
#[allow(dead_code)]
pub(crate) struct SymbolBatch {
    /// All vertices for this texture batch
    pub(crate) vertices: Vec<TextureVertex>,
    /// All indices for this texture batch
    pub(crate) indices: Vec<u32>,
    /// The texture bind group
    pub(crate) bind_group_idx: usize,
}

/// Simple Quadtree node for spatial indexing
#[allow(dead_code)]
pub(crate) struct QuadTreeNode {
    pub(crate) bounds: (f64, f64, f64, f64), // (min_x, min_y, max_x, max_y)
    pub(crate) feature_ids: Vec<i64>,
    pub(crate) children: Option<Box<[QuadTreeNode; 4]>>,
}

#[allow(dead_code)]
impl QuadTreeNode {
    pub(crate) fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        QuadTreeNode {
            bounds: (min_x, min_y, max_x, max_y),
            feature_ids: Vec::new(),
            children: None,
        }
    }

    /// Query features intersecting with viewport
    pub(crate) fn query(&self, viewport: (f64, f64, f64, f64), result: &mut Vec<i64>) {
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
    pub(crate) fn intersects(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
        a.0 <= b.2 && a.2 >= b.0 && a.1 <= b.3 && a.3 >= b.1
    }
}

/// Hash a slice of world points for cache key
#[allow(dead_code)]
pub(crate) fn hash_geometry(points: &[WorldPoint]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for p in points {
        ((p.x * 1_000_000.0) as i64).hash(&mut hasher);
        ((p.y * 1_000_000.0) as i64).hash(&mut hasher);
    }
    hasher.finish()
}

/// Cached GPU texture for a pattern fill (uses Repeat sampler)
pub(crate) struct PatternTexture {
    #[allow(dead_code)]
    pub(crate) texture: wgpu::Texture,
    pub(crate) bind_group: wgpu::BindGroup,
    /// Texture dimensions in pixels (matches tiling period exactly)
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Cached GPU texture for a symbol
pub(crate) struct SymbolTexture {
    #[allow(dead_code)]
    pub(crate) texture: wgpu::Texture,
    pub(crate) bind_group: wgpu::BindGroup,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Pivot position within texture (in pixels from top-left at render_scale)
    pub(crate) pivot_in_texture: (f32, f32),
    pub(crate) render_scale: f32,
}

/// Symbol instance to render
/// Memory optimized: uses interned SymbolId (4 bytes) instead of String (24 bytes)
#[derive(Clone, Copy)]
pub(crate) struct SymbolInstance {
    pub(crate) symbol_id: ferrite_render::SymbolId,
    pub(crate) screen_x: f32,
    pub(crate) screen_y: f32,
    pub(crate) scale: f32,
    pub(crate) rotation: f32,
}

/// Pending text label to render via egui painter overlay
pub(crate) struct TextLabel {
    pub(crate) screen_x: f32,
    pub(crate) screen_y: f32,
    pub(crate) text: String,
    pub(crate) font_size: f32,
    pub(crate) color: [f32; 4],
    #[allow(dead_code)]
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) h_align: ferrite_render::HAlign,
    pub(crate) v_align: ferrite_render::VAlign,
}

/// Grid-based text collision avoidance (S-100 Part 9: overplot removal)
pub(crate) struct TextCollisionGrid {
    pub(crate) occupied: FxHashSet<(i32, i32)>,
    pub(crate) cell_size: f32,
}

impl TextCollisionGrid {
    pub(crate) fn new(cell_size: f32) -> Self {
        Self {
            occupied: FxHashSet::with_capacity_and_hasher(2000, Default::default()),
            cell_size: cell_size.max(1.0),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.occupied.clear();
    }

    /// Try to place a text label. Returns true if space is available.
    pub(crate) fn try_place(&mut self, x: f32, y: f32, width: f32, height: f32) -> bool {
        let x0 = (x / self.cell_size).floor() as i32;
        let y0 = (y / self.cell_size).floor() as i32;
        let x1 = ((x + width) / self.cell_size).floor() as i32;
        let y1 = ((y + height) / self.cell_size).floor() as i32;

        // Check if any cell in the bounding box is occupied
        for gx in x0..=x1 {
            for gy in y0..=y1 {
                if self.occupied.contains(&(gx, gy)) {
                    return false;
                }
            }
        }

        // Claim cells
        for gx in x0..=x1 {
            for gy in y0..=y1 {
                self.occupied.insert((gx, gy));
            }
        }
        true
    }
}
