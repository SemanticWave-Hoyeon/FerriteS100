//! Check actual before/after application line traces against official PC ray lengths/bearings.
use anyhow::{ensure, Result};
use ferrite_kernel::geodesy::{inverse, GeographicPosition};
use serde_json::{json, Value};
fn position(v: &Value) -> Result<GeographicPosition> {
    let lat = v["y"].as_f64().unwrap();
    let lon = v["x"].as_f64().unwrap();
    GeographicPosition::new(lat, (lon + 180.).rem_euclid(360.) - 180.)
}
fn angle_error(a: f64, b: f64) -> f64 {
    ((a - b + 180.).rem_euclid(360.) - 180.).abs()
}
fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let diffs: Value = serde_json::from_slice(&std::fs::read(&a[1])?)?;
    let rays: Value = serde_json::from_slice(&std::fs::read(&a[2])?)?;
    let differences = diffs["first_40_differences"].as_array().unwrap();
    ensure!(
        differences.len() == diffs["changed_indices"].as_u64().unwrap() as usize,
        "Truncated trace differences"
    );
    let mut rows = Vec::new();
    for d in differences {
        let old = &d["left"]["Line"];
        let new = &d["right"]["Line"];
        let mut l = old.clone();
        let mut r = new.clone();
        l.as_object_mut().unwrap().remove("points");
        r.as_object_mut().unwrap().remove("points");
        ensure!(l == r, "Non-coordinate instruction changed");
        ensure!(
            new["screen_ray"].is_null(),
            "Expected an actual geographic ray"
        );
        let x = new["points"].as_array().unwrap();
        let y = old["points"].as_array().unwrap();
        ensure!(
            x.len() == 2 && y.len() == 2 && x[0] == y[0],
            "Ray origin/count changed"
        );
        let start = position(&x[0])?;
        let after = inverse(start, position(&x[1])?)?;
        let before = inverse(start, position(&y[1])?)?;
        let feature = new["feature_id"].as_i64().unwrap().to_string();
        let cell = new["cell_index"].as_u64().unwrap();
        let cell_name = match cell {
            0 => "101GB0050242H",
            1 => "101GB0050242G",
            _ => anyhow::bail!("Unexpected trace cell"),
        };
        let candidate = rays["rays"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| {
                q["id"].as_str() == Some(feature.as_str())
                    && q["source"].as_str().is_some_and(|s| s.contains(cell_name))
                    && q["direction_crs"] == "GeographicCRS"
                    && q["length_crs"] == "GeographicCRS"
                    && (q["length"].as_f64().unwrap() - after.distance_m).abs() < 1e-5
                    && angle_error(q["direction"].as_f64().unwrap(), after.initial_azimuth_deg)
                        < 1e-8
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No official geographic ray matches trace index {}",
                    d["index"]
                )
            })?;
        let length = candidate["length"].as_f64().unwrap();
        let direction = candidate["direction"].as_f64().unwrap();
        rows.push(json!({"index":d["index"],"cell":cell_name,"feature":feature,"pc_length_m":length,"pc_direction_deg":direction,
    "before_distance_error_m":before.distance_m-length,"after_distance_error_m":after.distance_m-length,
    "before_bearing_error_deg":angle_error(before.initial_azimuth_deg,direction),"after_bearing_error_deg":angle_error(after.initial_azimuth_deg,direction)}));
    }
    std::fs::write(&a[3], serde_json::to_vec_pretty(&rows)?)?;
    println!(
        "{} geographic ray endpoints match official PC distance and bearing",
        rows.len()
    );
    Ok(())
}
