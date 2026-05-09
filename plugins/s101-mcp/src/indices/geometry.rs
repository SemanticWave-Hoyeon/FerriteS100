//! Spatial index over feature bounding boxes.
//!
//! Uses an R-tree (`rstar`) keyed on per-feature world-coordinate AABBs, so
//! `feature_query_bbox` and `feature_nearby` cost ~log N, not N.
//!
//! Coordinates are the chart's native CRS (S-101 stores lon/lat in degrees
//! after applying CMFX/COMF), which is what every MCP query speaks.

use rstar::{primitives::GeomWithData, RTree, AABB};
use rustc_hash::FxHashMap;

use ferrite_s100_core::{Coordinate, S101Cell, SpatialPrimitiveType};

use super::feature::{FeatureId, FeatureIndex};

/// Axis-aligned bounding box (lon/lat in chart CRS).
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl Bbox {
    fn from_point(p: &Coordinate) -> Self {
        Bbox {
            min_lon: p.x,
            min_lat: p.y,
            max_lon: p.x,
            max_lat: p.y,
        }
    }

    fn expand_point(&mut self, p: &Coordinate) {
        if p.x < self.min_lon {
            self.min_lon = p.x;
        }
        if p.x > self.max_lon {
            self.max_lon = p.x;
        }
        if p.y < self.min_lat {
            self.min_lat = p.y;
        }
        if p.y > self.max_lat {
            self.max_lat = p.y;
        }
    }

    pub fn intersects(&self, other: &Bbox) -> bool {
        self.min_lon <= other.max_lon
            && self.max_lon >= other.min_lon
            && self.min_lat <= other.max_lat
            && self.max_lat >= other.min_lat
    }

    fn to_aabb(self) -> AABB<[f64; 2]> {
        AABB::from_corners([self.min_lon, self.min_lat], [self.max_lon, self.max_lat])
    }
}

type IndexedRect = GeomWithData<rstar::primitives::Rectangle<[f64; 2]>, FeatureId>;

pub struct GeometryIndex {
    /// R-tree indexing each feature's AABB. Features without resolvable
    /// geometry are still in the feature index but not here.
    tree: RTree<IndexedRect>,
    /// Per-feature bbox + a single representative point (centroid of bbox)
    /// used by `feature_nearby` distance queries and JSON responses.
    by_id: FxHashMap<FeatureId, FeatureGeometry>,
    /// Whole-cell extent. Empty when no geometry could be resolved.
    extent: Option<Bbox>,
}

#[derive(Debug, Clone, Copy)]
pub struct FeatureGeometry {
    pub bbox: Bbox,
    pub centroid: (f64, f64),
    /// Primitive type recorded for completeness — exposed for future
    /// per-primitive filtering. Currently unused by the 9-tool surface.
    #[allow(dead_code)]
    pub primitive: SpatialPrimitiveType,
}

impl GeometryIndex {
    pub fn build(cell: &S101Cell, _features: &FeatureIndex) -> Self {
        let mut by_id = FxHashMap::default();
        let mut rects: Vec<IndexedRect> = Vec::with_capacity(cell.features.len());
        let mut extent: Option<Bbox> = None;

        for (&fid, feature) in &cell.features {
            let bbox = match compute_feature_bbox(cell, feature) {
                Some(b) => b,
                None => continue,
            };
            let centroid = (
                (bbox.min_lon + bbox.max_lon) * 0.5,
                (bbox.min_lat + bbox.max_lat) * 0.5,
            );
            by_id.insert(
                fid,
                FeatureGeometry {
                    bbox,
                    centroid,
                    primitive: feature.primitive_type,
                },
            );
            rects.push(GeomWithData::new(
                rstar::primitives::Rectangle::from_corners(
                    [bbox.min_lon, bbox.min_lat],
                    [bbox.max_lon, bbox.max_lat],
                ),
                fid,
            ));
            extent = Some(match extent {
                Some(mut e) => {
                    e.expand_point(&Coordinate::new(bbox.min_lon, bbox.min_lat));
                    e.expand_point(&Coordinate::new(bbox.max_lon, bbox.max_lat));
                    e
                }
                None => bbox,
            });
        }

        let tree = RTree::bulk_load(rects);

        GeometryIndex {
            tree,
            by_id,
            extent,
        }
    }

    pub fn extent(&self) -> Option<Bbox> {
        self.extent
    }

    pub fn get(&self, id: FeatureId) -> Option<&FeatureGeometry> {
        self.by_id.get(&id)
    }

    pub fn indexed_count(&self) -> usize {
        self.by_id.len()
    }

    /// All feature ids whose AABB intersects the query bbox.
    pub fn query_bbox(&self, q: Bbox) -> Vec<FeatureId> {
        self.tree
            .locate_in_envelope_intersecting(&q.to_aabb())
            .map(|r| r.data)
            .collect()
    }

    /// Feature ids within `radius_m` of the (lat, lon) point, ordered by
    /// distance (nearest first). Distance is approximate haversine — fine
    /// for the ranges plan2 cares about (≤ a few km).
    pub fn query_nearby(&self, lat: f64, lon: f64, radius_m: f64) -> Vec<(FeatureId, f64)> {
        // Use a generous bbox prefilter, then refine with haversine.
        let deg_per_m_lat = 1.0 / 111_320.0;
        let cos_lat = lat.to_radians().cos().max(0.001);
        let deg_per_m_lon = deg_per_m_lat / cos_lat;
        let dlat = radius_m * deg_per_m_lat;
        let dlon = radius_m * deg_per_m_lon;
        let prefilter = Bbox {
            min_lon: lon - dlon,
            min_lat: lat - dlat,
            max_lon: lon + dlon,
            max_lat: lat + dlat,
        };
        let mut hits: Vec<(FeatureId, f64)> = self
            .query_bbox(prefilter)
            .into_iter()
            .filter_map(|fid| {
                let g = self.by_id.get(&fid)?;
                let d = haversine_m(lat, lon, g.centroid.1, g.centroid.0);
                if d <= radius_m {
                    Some((fid, d))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        hits
    }
}

fn compute_feature_bbox(
    cell: &S101Cell,
    feature: &ferrite_s100_core::FeatureRecord,
) -> Option<Bbox> {
    let mut bbox: Option<Bbox> = None;
    let mut update = |c: &Coordinate| match &mut bbox {
        Some(b) => b.expand_point(c),
        None => bbox = Some(Bbox::from_point(c)),
    };

    for spas in &feature.spatial_associations {
        let key = spas.spatial_id.key();
        if let Some(pt) = cell.points.get(&key) {
            update(&pt.position);
            continue;
        }
        if let Some(mp) = cell.multi_points.get(&key) {
            for p in &mp.positions {
                update(p);
            }
            continue;
        }
        if let Some(curve) = cell.curves.get(&key) {
            for p in curve.all_positions() {
                update(&p);
            }
            continue;
        }
        if let Some(composite) = cell.composite_curves.get(&key) {
            for sub in &composite.curves {
                if let Some(curve) = cell.curves.get(&sub.curve_id.key()) {
                    for p in curve.all_positions() {
                        update(&p);
                    }
                }
            }
            continue;
        }
        if let Some(surface) = cell.surfaces.get(&key) {
            for oc in &surface.exterior_ring {
                if let Some(curve) = cell.curves.get(&oc.curve_id.key()) {
                    for p in curve.all_positions() {
                        update(&p);
                    }
                } else if let Some(comp) = cell.composite_curves.get(&oc.curve_id.key()) {
                    for sub in &comp.curves {
                        if let Some(curve) = cell.curves.get(&sub.curve_id.key()) {
                            for p in curve.all_positions() {
                                update(&p);
                            }
                        }
                    }
                }
            }
            continue;
        }
    }

    bbox
}

/// Haversine great-circle distance in metres.
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_R_M: f64 = 6_371_008.8;
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_R_M * a.sqrt().atan2((1.0 - a).sqrt())
}
