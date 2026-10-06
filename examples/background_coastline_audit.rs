use ferrite_render::BackgroundCoastlines;
use serde_json::{json, Value};
use std::time::Instant;
fn main() {
    let input: Value =
        serde_json::from_str(include_str!("../assets/ne_10m_coastline.geojson")).unwrap();
    let lines: Vec<Vec<[f64; 2]>> = input["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            assert_eq!(f["geometry"]["type"], "LineString");
            f["geometry"]["coordinates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| [p[0].as_f64().unwrap(), p[1].as_f64().unwrap()])
                .collect()
        })
        .collect();
    let start = Instant::now();
    let index = BackgroundCoastlines::new(lines.clone()).unwrap();
    let build_seconds = start.elapsed().as_secs_f64();
    let mut rows = vec![];
    for (name, view) in [
        ("channel", [-5., 48., 1., 51.]),
        ("portsmouth", [-1.2, 50.7, -1., 50.9]),
        ("dateline", [178., -20., 182., 20.]),
        ("global", [-180., -90., 180., 90.]),
    ] {
        let mut expected = vec![];
        let mut actual = vec![];
        let touches = |a: [f64; 2], b: [f64; 2], off: f64| {
            a[0].min(b[0]) + off <= view[2]
                && a[0].max(b[0]) + off >= view[0]
                && a[1].min(b[1]) <= view[3]
                && a[1].max(b[1]) >= view[1]
        };
        let start = Instant::now();
        for off in [-360., 0., 360.] {
            for line in &lines {
                for w in line.windows(2) {
                    if touches(w[0], w[1], off) {
                        expected.push((off, [w[0], w[1]]));
                    }
                }
            }
        }
        let exhaustive_seconds = start.elapsed().as_secs_f64();
        let start = Instant::now();
        let mut visited = 0;
        for off in [-360., 0., 360.] {
            for chunk in index.visible_chunks(view, off) {
                visited += chunk.points.len() - 1;
                for w in chunk.points.windows(2) {
                    if touches(w[0], w[1], off) {
                        actual.push((off, [w[0], w[1]]));
                    }
                }
            }
        }
        let indexed_seconds = start.elapsed().as_secs_f64();
        assert_eq!(actual, expected);
        if name == "channel" || name == "portsmouth" {
            assert!(visited < index.segments() / 20);
        }
        rows.push(json!({"view":name,"bounds":view,"exhaustive_segment_tests":index.segments()*3,"indexed_segment_tests":visited,"matching_segments":expected.len(),"exact_endpoints_and_order_equal":true,"exhaustive_seconds":exhaustive_seconds,"indexed_seconds":indexed_seconds}));
    }
    let output = json!({"total_segments":index.segments(),"chunks":index.chunk_count(),"index_build_seconds":build_seconds,"reference_data_not_enc":true,"queries":rows});
    std::fs::write(
        std::env::args().nth(1).expect("output JSON"),
        serde_json::to_vec_pretty(&output).unwrap(),
    )
    .unwrap();
}
