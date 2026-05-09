//! Parse the embedded Natural Earth coastline GeoJSON into line segments.
//!
//! The dataset is bundled into the binary via `include_str!` so the world-map
//! background renders even before any chart is loaded. Output is a `Vec` of
//! polylines, each a list of `[longitude, latitude]` pairs.

/// Embedded Natural Earth 110m coastline GeoJSON (~140 KB).
/// Source: https://www.naturalearthdata.com/ (Public Domain)
const WORLD_MAP_GEOJSON: &str = include_str!("../../assets/ne_110m_coastline.geojson");

/// Parse all coastlines as polylines. Polygons are rendered as their exterior
/// ring only (this is the world-map background, not a fill operation).
pub fn parse_world_map_coastlines() -> Vec<Vec<[f64; 2]>> {
    let parsed: serde_json::Value = match serde_json::from_str(WORLD_MAP_GEOJSON) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse world map GeoJSON: {}", e);
            return Vec::new();
        }
    };

    let mut coastlines = Vec::new();

    if let Some(features) = parsed.get("features").and_then(|f| f.as_array()) {
        for feature in features {
            let geometry = match feature.get("geometry") {
                Some(g) => g,
                None => continue,
            };
            let geo_type = geometry.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let coords = match geometry.get("coordinates") {
                Some(c) => c,
                None => continue,
            };

            match geo_type {
                "LineString" => {
                    if let Some(line) = parse_coord_array(coords) {
                        if line.len() >= 2 {
                            coastlines.push(line);
                        }
                    }
                }
                "MultiLineString" => {
                    if let Some(lines) = coords.as_array() {
                        for line_coords in lines {
                            if let Some(line) = parse_coord_array(line_coords) {
                                if line.len() >= 2 {
                                    coastlines.push(line);
                                }
                            }
                        }
                    }
                }
                "Polygon" => {
                    // Extract exterior ring as a line
                    if let Some(rings) = coords.as_array() {
                        if let Some(exterior) = rings.first() {
                            if let Some(line) = parse_coord_array(exterior) {
                                if line.len() >= 2 {
                                    coastlines.push(line);
                                }
                            }
                        }
                    }
                }
                "MultiPolygon" => {
                    if let Some(polygons) = coords.as_array() {
                        for polygon in polygons {
                            if let Some(rings) = polygon.as_array() {
                                if let Some(exterior) = rings.first() {
                                    if let Some(line) = parse_coord_array(exterior) {
                                        if line.len() >= 2 {
                                            coastlines.push(line);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    coastlines
}

/// Parse a GeoJSON coordinate array `[[lon, lat], ...]` into `Vec<[f64; 2]>`.
fn parse_coord_array(value: &serde_json::Value) -> Option<Vec<[f64; 2]>> {
    let arr = value.as_array()?;
    let mut points = Vec::with_capacity(arr.len());
    for coord in arr {
        let pair = coord.as_array()?;
        if pair.len() >= 2 {
            let lon = pair[0].as_f64()?;
            let lat = pair[1].as_f64()?;
            points.push([lon, lat]);
        }
    }
    Some(points)
}
