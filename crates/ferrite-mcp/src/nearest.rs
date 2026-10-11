//! `feature_nearest`: the nearest features to a position across every loaded
//! S-101 dataset in one call, measured to the feature geometry itself (point,
//! curve segment, surface boundary; zero inside a surface), so a client never
//! has to page through `feature_query_by_type` per dataset.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

use ferrite_s100_core::{Coordinate, FeatureRecord, S101Cell};

use crate::indices::{feature::FeatureId, geometry::Bbox, Indices};

const EARTH_R_M: f64 = 6_371_008.8;
const METRES_PER_NM: f64 = 1852.0;
/// Without a type filter the search is bounded to this radius by default.
const DEFAULT_UNTYPED_RADIUS_M: f64 = 5_000.0;
/// Bounded work per call (features examined across all datasets).
const MAX_CANDIDATES: usize = 200_000;

/// Parses "50°37′24.00″N, 001°09′25.21″W", "50 37.4 N 1 9.42 W",
/// "50.6233, -1.1570" and similar. Hemisphere letters decide the axis when
/// present; otherwise the first value is latitude.
pub fn parse_position(text: &str) -> Result<(f64, f64)> {
    ensure!(text.len() <= 200, "position text too long");
    let mut parts: Vec<(f64, Option<char>)> = Vec::new();
    let mut numbers: Vec<f64> = Vec::new();
    let mut negative = false;
    let mut token = String::new();
    let flush_number = |token: &mut String, numbers: &mut Vec<f64>| -> Result<()> {
        if !token.is_empty() {
            numbers.push(token.parse().context("invalid number in position")?);
            token.clear();
        }
        Ok(())
    };
    let finish = |numbers: &mut Vec<f64>,
                  negative: &mut bool,
                  hemisphere: Option<char>,
                  parts: &mut Vec<(f64, Option<char>)>|
     -> Result<()> {
        if numbers.is_empty() {
            return Ok(());
        }
        ensure!(numbers.len() <= 3, "too many numbers in one coordinate");
        let scale = [1.0, 60.0, 3600.0];
        let mut value = 0.0;
        for (i, n) in numbers.iter().enumerate() {
            ensure!(*n >= 0.0, "negative component");
            if i > 0 {
                ensure!(*n < 60.0, "minutes/seconds must be below 60");
            }
            value += n / scale[i];
        }
        if *negative {
            value = -value;
        }
        parts.push((value, hemisphere));
        numbers.clear();
        *negative = false;
        Ok(())
    };
    for c in text.chars() {
        match c {
            '0'..='9' | '.' => token.push(c),
            '-' | '−' if token.is_empty() && numbers.is_empty() => negative = true,
            'N' | 'S' | 'E' | 'W' | 'n' | 's' | 'e' | 'w' => {
                flush_number(&mut token, &mut numbers)?;
                let h = c.to_ascii_uppercase();
                if numbers.is_empty() {
                    // A leading hemisphere ("N 50 37 24") applies to what follows.
                    parts.push((f64::NAN, Some(h)));
                } else {
                    finish(&mut numbers, &mut negative, Some(h), &mut parts)?;
                }
            }
            ',' | ';' => {
                flush_number(&mut token, &mut numbers)?;
                finish(&mut numbers, &mut negative, None, &mut parts)?;
            }
            _ => flush_number(&mut token, &mut numbers)?, // °, ′, ″, ', ", spaces
        }
    }
    flush_number(&mut token, &mut numbers)?;
    finish(&mut numbers, &mut negative, None, &mut parts)?;
    // Attach a leading hemisphere marker to the value after it.
    let mut values: Vec<(f64, Option<char>)> = Vec::new();
    let mut pending = None;
    for (v, h) in parts {
        if v.is_nan() {
            pending = h;
        } else {
            values.push((v, h.or(pending.take())));
        }
    }
    if values.len() == 6 || values.len() == 4 {
        bail!("give each coordinate as one value or with its own separator");
    }
    ensure!(values.len() == 2, "expected a latitude and a longitude");
    let signed = |(v, h): (f64, Option<char>)| match h {
        Some('S') | Some('W') => -v.abs(),
        _ => v,
    };
    let (a, b) = (values[0], values[1]);
    let (lat, lon) = match (a.1, b.1) {
        (Some('E') | Some('W'), _) | (_, Some('N') | Some('S')) => (signed(b), signed(a)),
        _ => (signed(a), signed(b)),
    };
    ensure!((-90.0..=90.0).contains(&lat), "latitude out of range");
    ensure!((-180.0..=180.0).contains(&lon), "longitude out of range");
    Ok((lat, lon))
}

/// "50°37′24.00″N" / "001°09′25.21″W".
pub fn format_dms(value: f64, latitude: bool) -> String {
    let hemisphere = match (latitude, value < 0.0) {
        (true, false) => 'N',
        (true, true) => 'S',
        (false, false) => 'E',
        (false, true) => 'W',
    };
    let total = (value.abs() * 360_000.0).round() as u64; // hundredths of a second
    let (deg, rem) = (total / 360_000, total % 360_000);
    let (min, cs) = (rem / 6_000, rem % 6_000);
    if latitude {
        format!(
            "{deg:02}°{min:02}′{:02}.{:02}″{hemisphere}",
            cs / 100,
            cs % 100
        )
    } else {
        format!(
            "{deg:03}°{min:02}′{:02}.{:02}″{hemisphere}",
            cs / 100,
            cs % 100
        )
    }
}

/// Local east/north metres around the query point (accurate to well under
/// 1% for the tens of kilometres a nearest-feature question spans).
struct Local {
    lat0: f64,
    lon0: f64,
    cos0: f64,
}
impl Local {
    fn new(lat0: f64, lon0: f64) -> Self {
        Self {
            lat0,
            lon0,
            cos0: lat0.to_radians().cos(),
        }
    }
    fn xy(&self, c: &Coordinate) -> (f64, f64) {
        let mut dlon = c.x - self.lon0;
        if dlon > 180.0 {
            dlon -= 360.0;
        } else if dlon < -180.0 {
            dlon += 360.0;
        }
        (
            dlon.to_radians() * self.cos0 * EARTH_R_M,
            (c.y - self.lat0).to_radians() * EARTH_R_M,
        )
    }
    fn back(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (
            self.lat0 + (y / EARTH_R_M).to_degrees(),
            self.lon0 + (x / (EARTH_R_M * self.cos0.max(1e-9))).to_degrees(),
        )
    }
}

fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_R_M * a.sqrt().atan2((1.0 - a).sqrt())
}
fn bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Point, curve and surface-ring parts of a feature, resolved from the cell.
#[derive(Default)]
struct Parts {
    points: Vec<Coordinate>,
    lines: Vec<Vec<Coordinate>>,
    rings: Vec<Vec<Coordinate>>,
}
fn curve_positions(cell: &S101Cell, key: i64, out: &mut Vec<Coordinate>) -> bool {
    if let Some(c) = cell.curves.get(&key) {
        out.extend(c.all_positions());
        return true;
    }
    if let Some(comp) = cell.composite_curves.get(&key) {
        for sub in &comp.curves {
            if let Some(c) = cell.curves.get(&sub.curve_id.key()) {
                out.extend(c.all_positions());
            }
        }
        return true;
    }
    false
}
fn feature_parts(cell: &S101Cell, f: &FeatureRecord) -> Parts {
    let mut parts = Parts::default();
    for spas in &f.spatial_associations {
        let key = spas.spatial_id.key();
        if let Some(p) = cell.points.get(&key) {
            parts.points.push(p.position);
        } else if let Some(mp) = cell.multi_points.get(&key) {
            parts.points.extend(mp.positions.iter().copied());
        } else if let Some(s) = cell.surfaces.get(&key) {
            let mut ring = Vec::new();
            for oc in &s.exterior_ring {
                curve_positions(cell, oc.curve_id.key(), &mut ring);
            }
            if ring.len() >= 3 {
                parts.rings.push(ring);
            }
        } else {
            let mut line = Vec::new();
            if curve_positions(cell, key, &mut line) && !line.is_empty() {
                parts.lines.push(line);
            }
        }
    }
    parts
}

/// Distance (m) and the closest point (lat, lon) of a feature to the origin.
fn feature_distance(local: &Local, parts: &Parts) -> Option<(f64, (f64, f64), bool)> {
    let mut best: Option<(f64, (f64, f64))> = None;
    let mut consider = |d: f64, at: (f64, f64)| {
        if best.is_none_or(|(b, _)| d < b) {
            best = Some((d, at));
        }
    };
    for p in &parts.points {
        consider(haversine_m(local.lat0, local.lon0, p.y, p.x), (p.y, p.x));
    }
    let segment = |a: (f64, f64), b: (f64, f64)| -> (f64, (f64, f64)) {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        let t = if len2 > 0.0 {
            (-(a.0 * dx + a.1 * dy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let q = (a.0 + t * dx, a.1 + t * dy);
        (q.0.hypot(q.1), q)
    };
    let mut inside = false;
    for (polyline, closed) in parts
        .lines
        .iter()
        .map(|l| (l, false))
        .chain(parts.rings.iter().map(|r| (r, true)))
    {
        let xy: Vec<(f64, f64)> = polyline.iter().map(|c| local.xy(c)).collect();
        if xy.len() == 1 {
            consider(xy[0].0.hypot(xy[0].1), local.back(xy[0]));
        }
        for w in xy.windows(2) {
            let (d, q) = segment(w[0], w[1]);
            consider(d, local.back(q));
        }
        if closed {
            // Even-odd test of the origin against the ring.
            let mut crossing = false;
            for i in 0..xy.len() {
                let (a, b) = (xy[i], xy[(i + 1) % xy.len()]);
                if (a.1 > 0.0) != (b.1 > 0.0) && 0.0 < a.0 + (0.0 - a.1) * (b.0 - a.0) / (b.1 - a.1)
                {
                    crossing = !crossing;
                }
            }
            inside |= crossing;
        }
    }
    let (d, at) = best?;
    Some(if inside {
        (0.0, (local.lat0, local.lon0), true)
    } else {
        (d, at, false)
    })
}

fn attribute_text(f: &FeatureRecord, code: &str) -> Option<String> {
    f.attributes
        .iter()
        .find(|a| a.code.as_deref() == Some(code))
        .map(|a| match &a.value {
            Some(v) => format!("{v}"),
            None => a.atvl.clone(),
        })
        .filter(|s| !s.is_empty())
}

/// (distance m, closest point, inside, dataset id, index, feature id)
type Hit<'a> = (f64, (f64, f64), bool, &'a str, &'a Indices, FeatureId);

pub struct Candidate<'a> {
    pub dataset_id: &'a str,
    pub index: &'a Indices,
}

#[derive(serde::Deserialize)]
struct Args {
    #[serde(default)]
    position: Option<String>,
    #[serde(default)]
    lat: Option<f64>,
    #[serde(default)]
    lon: Option<f64>,
    #[serde(default, rename = "type")]
    type_code: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    max_radius_m: Option<f64>,
    #[serde(default = "default_true")]
    merge_duplicates: bool,
    #[serde(default)]
    dataset_id: Option<String>,
}
fn default_limit() -> usize {
    5
}
fn default_true() -> bool {
    true
}

pub fn feature_nearest(datasets: &[Candidate<'_>], args: Value) -> Result<Value> {
    let a: Args = serde_json::from_value(args)?;
    let _ = &a.dataset_id; // selection is done by the caller
    ensure!((1..=50).contains(&a.limit), "limit must be 1..50");
    let (lat, lon) = match (&a.position, a.lat, a.lon) {
        (Some(text), None, None) => parse_position(text)?,
        (None, Some(lat), Some(lon)) => (lat, lon),
        _ => bail!("give either `position` (text) or both `lat` and `lon`"),
    };
    ensure!(
        lat.is_finite() && (-90.0..=90.0).contains(&lat),
        "latitude out of range"
    );
    ensure!(
        lon.is_finite() && (-180.0..=180.0).contains(&lon),
        "longitude out of range"
    );
    let radius = match (a.max_radius_m, &a.type_code) {
        (Some(r), _) => {
            ensure!(
                r.is_finite() && r > 0.0 && r <= 1_000_000.0,
                "invalid max_radius_m"
            );
            Some(r)
        }
        (None, None) => Some(DEFAULT_UNTYPED_RADIUS_M),
        (None, Some(_)) => None,
    };
    let local = Local::new(lat, lon);
    let prefilter = radius.map(|r| {
        let dlat = (r / EARTH_R_M).to_degrees();
        let dlon = dlat / local.cos0.max(0.001);
        Bbox {
            min_lon: lon - dlon,
            min_lat: lat - dlat,
            max_lon: lon + dlon,
            max_lat: lat + dlat,
        }
    });
    let mut examined = 0usize;
    let mut hits: Vec<Hit<'_>> = Vec::new();
    for d in datasets {
        let ids: Vec<FeatureId> = match (&a.type_code, prefilter) {
            (Some(t), Some(b)) => d
                .index
                .feature
                .ids_by_type(t)
                .iter()
                .copied()
                .filter(|id| {
                    d.index
                        .geometry
                        .get(*id)
                        .is_some_and(|g| g.bbox.intersects(&b))
                })
                .collect(),
            (Some(t), None) => d.index.feature.ids_by_type(t).to_vec(),
            (None, Some(b)) => d.index.geometry.query_bbox(b),
            (None, None) => unreachable!("untyped searches always have a radius"),
        };
        examined += ids.len();
        ensure!(
            examined <= MAX_CANDIDATES,
            "too many candidate features; add `type` or a smaller `max_radius_m`"
        );
        for id in ids {
            let Some(f) = d.index.cell.features.get(&id) else {
                continue;
            };
            let parts = feature_parts(&d.index.cell, f);
            let Some((dist, at, inside)) = feature_distance(&local, &parts) else {
                continue;
            };
            if radius.is_some_and(|r| dist > r) {
                continue;
            }
            hits.push((dist, at, inside, d.dataset_id, d.index, id));
        }
    }
    hits.sort_by(|x, y| x.0.total_cmp(&y.0));
    // The same real-world object often appears in overlapping cells of
    // different scales: report it once, listing the other datasets.
    let mut results: Vec<Value> = Vec::new();
    let mut seen: Vec<(String, (f64, f64), usize)> = Vec::new();
    for (dist, at, inside, dataset_id, index, id) in hits {
        let f = &index.cell.features[&id];
        let code = f.feature_code.clone().unwrap_or_default();
        if a.merge_duplicates {
            if let Some((_, _, slot)) = seen
                .iter()
                .find(|(c, p, _)| *c == code && haversine_m(p.0, p.1, at.0, at.1) < 15.0)
            {
                if let Some(arr) = results[*slot]["also_in"].as_array_mut() {
                    arr.push(json!({"dataset_id": dataset_id, "id": id}));
                }
                continue;
            }
        }
        if results.len() == a.limit {
            continue;
        }
        let position = (f.primitive_type == ferrite_s100_core::SpatialPrimitiveType::Point)
            .then(|| feature_parts(&index.cell, f).points.first().copied())
            .flatten()
            .map(|p| {
                json!({"lat": p.y, "lon": p.x, "lat_dms": format_dms(p.y, true), "lon_dms": format_dms(p.x, false)})
            });
        let attributes: Vec<Value> = f
            .attributes
            .iter()
            .filter(|at| at.paix == 0)
            .take(12)
            .map(|at| {
                let value = match &at.value {
                    Some(v) => format!("{v}"),
                    None => at.atvl.clone(),
                };
                json!({"code": at.code, "value": value})
            })
            .collect();
        seen.push((code.clone(), at, results.len()));
        results.push(json!({
            "dataset_id": dataset_id,
            "id": id,
            "type": code,
            "primitive": format!("{:?}", f.primitive_type),
            "name": attribute_text(f, "name"),
            "distance_m": (dist * 10.0).round() / 10.0,
            "distance_nm": (dist / METRES_PER_NM * 1000.0).round() / 1000.0,
            "bearing_deg_true": if inside { Value::Null } else { json!((bearing_deg(lat, lon, at.0, at.1) * 10.0).round() / 10.0) },
            "inside_feature": inside,
            "closest_point": {"lat": at.0, "lon": at.1, "lat_dms": format_dms(at.0, true), "lon_dms": format_dms(at.1, false)},
            "position": position,
            "attributes": attributes,
            "also_in": [],
        }));
    }
    let evidence: Vec<Value> = results
        .iter()
        .map(|r| json!({"kind": "feature", "dataset_id": r["dataset_id"], "id": r["id"], "type": r["type"]}))
        .collect();
    Ok(json!({
        "query": {"lat": lat, "lon": lon, "lat_dms": format_dms(lat, true), "lon_dms": format_dms(lon, false),
                  "type": a.type_code, "max_radius_m": radius, "datasets_searched": datasets.len()},
        "method": "Distance to the feature geometry (points: great-circle; curves and surface boundaries: local plane around the query, under 1% error within tens of km; 0 inside a surface). Not for navigation.",
        "matched": results.len(),
        "features": results,
        "evidence": evidence,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positions_parse_from_dms_and_decimal_forms() {
        let (lat, lon) = parse_position("50°37′24.00″N, 001°09′25.21″W").unwrap();
        assert!((lat - (50.0 + 37.0 / 60.0 + 24.0 / 3600.0)).abs() < 1e-9);
        assert!((lon + (1.0 + 9.0 / 60.0 + 25.21 / 3600.0)).abs() < 1e-9);
        for text in [
            "50 37 24 N 1 9 25.21 W",
            "N 50 37 24, W 1 9 25.21",
            "1°09′25.21″W 50°37′24″N",
            "50.623333, -1.157003",
            "50°37.4'N 1°09.4202'W",
        ] {
            let (a, b) = parse_position(text).unwrap();
            assert!(
                (a - 50.6233).abs() < 1e-3 && (b + 1.157).abs() < 1e-3,
                "{text}"
            );
        }
        assert!(parse_position("95 N, 1 W").is_err());
        assert!(parse_position("50 61 0 N, 1 W").is_err());
        assert!(parse_position("hello").is_err());
        assert_eq!(format_dms(50.623333333, true), "50°37′24.00″N");
        assert_eq!(format_dms(-1.157002778, false), "001°09′25.21″W");
    }
    #[test]
    fn distance_uses_segments_and_is_zero_inside_surfaces() {
        let local = Local::new(50.0, -1.0);
        let c = |lat: f64, lon: f64| Coordinate::new(lon, lat);
        // A line 0.01° north of the origin: ~1112 m away, closest point due north.
        let line = Parts {
            lines: vec![vec![c(50.01, -1.1), c(50.01, -0.9)]],
            ..Default::default()
        };
        let (d, at, inside) = feature_distance(&local, &line).unwrap();
        assert!((d - 1111.9).abs() < 2.0 && !inside, "{d}");
        assert!((at.1 + 1.0).abs() < 1e-6);
        let square = Parts {
            rings: vec![vec![
                c(49.9, -1.1),
                c(49.9, -0.9),
                c(50.1, -0.9),
                c(50.1, -1.1),
                c(49.9, -1.1),
            ]],
            ..Default::default()
        };
        assert_eq!(feature_distance(&local, &square).unwrap().0, 0.0);
        let point = Parts {
            points: vec![c(50.0, -0.99)],
            ..Default::default()
        };
        let (d, _, _) = feature_distance(&local, &point).unwrap();
        assert!((d - 715.0).abs() < 2.0, "{d}");
    }
}
