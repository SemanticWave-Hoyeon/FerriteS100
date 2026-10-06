use ferrite_kernel::{geodesy::GeographicPosition, rhumb::RhumbSegment};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let inputs: Vec<[f64; 4]> = serde_json::from_slice(&std::fs::read(&args[1]).unwrap()).unwrap();
    let rows: Vec<_> = inputs.into_iter().map(|p| {
        let r = RhumbSegment::new(GeographicPosition::new(p[0], p[1]).unwrap(), GeographicPosition::new(p[2], p[3]).unwrap()).unwrap();
        serde_json::json!({"input":p,"bearing_deg":r.bearing_deg(),"distance_m":r.distance_m().unwrap()})
    }).collect();
    std::fs::write(&args[2], serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
}
