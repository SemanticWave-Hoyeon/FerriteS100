//! Spatial record types for S-100
//!
//! Contains Point, Curve, Surface, and other geometric primitives.

use geo_types::{Coord, LineString};

/// Spatial primitive type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialPrimitiveType {
    Point,
    MultiPoint,
    Curve,
    CompositeCurve,
    Surface,
    NoGeometry,
}

/// Record identifier (RCID + RCNM)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId {
    /// Record name (type indicator)
    pub rcnm: u8,
    /// Record identifier (unique within type)
    pub rcid: u32,
}

impl RecordId {
    pub fn new(rcnm: u8, rcid: u32) -> Self {
        RecordId { rcnm, rcid }
    }

    /// Create key for hashmap lookup
    pub fn key(&self) -> i64 {
        ((self.rcnm as i64) << 32) | (self.rcid as i64)
    }
}

/// Coordinate with optional depth (NaN = no depth)
/// Memory optimized: 24 bytes instead of 32 bytes with Option<f64>
#[derive(Debug, Clone, Copy)]
pub struct Coordinate {
    pub x: f64, // Longitude
    pub y: f64, // Latitude
    z: f64,     // Depth/height (NaN = not present)
}

impl Coordinate {
    pub fn new(x: f64, y: f64) -> Self {
        Coordinate { x, y, z: f64::NAN }
    }

    pub fn new_3d(x: f64, y: f64, z: f64) -> Self {
        Coordinate { x, y, z }
    }

    /// Get depth value if present (non-NaN)
    #[inline]
    pub fn depth(&self) -> Option<f64> {
        if self.z.is_nan() {
            None
        } else {
            Some(self.z)
        }
    }

    /// Check if coordinate has depth value
    #[inline]
    pub fn has_depth(&self) -> bool {
        !self.z.is_nan()
    }

    /// Get raw z value (may be NaN)
    #[inline]
    pub fn z_raw(&self) -> f64 {
        self.z
    }
}

impl From<Coordinate> for Coord<f64> {
    fn from(c: Coordinate) -> Self {
        Coord { x: c.x, y: c.y }
    }
}

/// Point record
#[derive(Debug, Clone)]
pub struct PointRecord {
    pub id: RecordId,
    pub position: Coordinate,
    pub update_instruction: u8,
}

/// Multi-point record (for soundings)
#[derive(Debug, Clone)]
pub struct MultiPointRecord {
    pub id: RecordId,
    pub positions: Vec<Coordinate>,
    pub update_instruction: u8,
}

/// Curve segment type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentType {
    Line,
    Arc,
    Ellipse,
    Clothoid,
}

/// Curve segment
#[derive(Debug, Clone)]
pub struct CurveSegment {
    pub segment_type: SegmentType,
    pub positions: Vec<Coordinate>,
}

/// Curve record
#[derive(Debug, Clone)]
pub struct CurveRecord {
    pub id: RecordId,
    pub segments: Vec<CurveSegment>,
    /// Start point record reference
    pub start_point: Option<RecordId>,
    /// End point record reference
    pub end_point: Option<RecordId>,
    pub update_instruction: u8,
}

impl CurveRecord {
    /// Get all positions in the curve as an iterator (avoids allocation)
    pub fn positions_iter(&self) -> impl Iterator<Item = &Coordinate> + '_ {
        self.segments.iter().flat_map(|s| s.positions.iter())
    }

    /// Get all positions in the curve (allocates Vec)
    pub fn all_positions(&self) -> Vec<Coordinate> {
        self.positions_iter().cloned().collect()
    }

    /// Convert to geo-types LineString (single allocation)
    pub fn to_linestring(&self) -> LineString<f64> {
        // Direct conversion without intermediate Vec<Coordinate>
        let coords: Vec<Coord<f64>> = self
            .positions_iter()
            .map(|c| Coord { x: c.x, y: c.y })
            .collect();
        LineString::new(coords)
    }
}

/// Oriented curve (curve with direction)
#[derive(Debug, Clone)]
pub struct OrientedCurve {
    pub curve_id: RecordId,
    pub orientation: bool, // true = forward, false = reverse
}

/// Composite curve record
#[derive(Debug, Clone)]
pub struct CompositeCurveRecord {
    pub id: RecordId,
    pub curves: Vec<OrientedCurve>,
    pub update_instruction: u8,
}

/// Ring (exterior or interior boundary)
#[derive(Debug, Clone)]
pub struct Ring {
    pub curves: Vec<OrientedCurve>,
    pub is_exterior: bool,
}

/// Surface record
#[derive(Debug, Clone)]
pub struct SurfaceRecord {
    pub id: RecordId,
    pub exterior_ring: Vec<OrientedCurve>,
    pub interior_rings: Vec<Vec<OrientedCurve>>,
    pub update_instruction: u8,
}
