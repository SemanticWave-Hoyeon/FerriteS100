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

/// Coordinate with optional depth
#[derive(Debug, Clone, Copy)]
pub struct Coordinate {
    pub x: f64, // Longitude
    pub y: f64, // Latitude
    pub z: Option<f64>, // Depth/height
}

impl Coordinate {
    pub fn new(x: f64, y: f64) -> Self {
        Coordinate { x, y, z: None }
    }

    pub fn new_3d(x: f64, y: f64, z: f64) -> Self {
        Coordinate { x, y, z: Some(z) }
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
    /// Get all positions in the curve
    pub fn all_positions(&self) -> Vec<Coordinate> {
        self.segments
            .iter()
            .flat_map(|s| s.positions.iter().cloned())
            .collect()
    }

    /// Convert to geo-types LineString
    pub fn to_linestring(&self) -> LineString<f64> {
        let coords: Vec<Coord<f64>> = self
            .all_positions()
            .into_iter()
            .map(|c| c.into())
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
