//! Headless sparse/dense selection cost and exact-result oracle; synthetic data.
use ferrite_render::{
    hit_geometry, AreaInstruction, DrawingInstruction, GeoBounds, LineInstruction, Scaler,
    ScreenPoint, SelectionIndex, Viewport, WorldPoint,
};
use std::time::Instant;
fn main() -> anyhow::Result<()> {
    let output = std::env::args().nth(1).expect("output JSON");
    let mut rows = Vec::new();
    for (kind, count) in [("sparse", 128), ("sparse", 4096), ("overlapping", 1024)] {
        let instructions: Vec<_> = (0..count)
            .map(|n| {
                if kind == "sparse" {
                    let x = (n % 64) as f64;
                    let y = (n / 64) as f64;
                    DrawingInstruction::Line(LineInstruction::new(vec![
                        WorldPoint::new(x + 0.1, y + 0.1),
                        WorldPoint::new(x + 0.5, y + 0.5),
                    ]))
                } else {
                    DrawingInstruction::Area(AreaInstruction::new(vec![
                        WorldPoint::new(0., 0.),
                        WorldPoint::new(64., 0.),
                        WorldPoint::new(64., 64.),
                        WorldPoint::new(0., 64.),
                    ]))
                }
            })
            .collect();
        let s = Scaler::new(
            GeoBounds::new(0., 0., 64., 64.),
            Viewport::new(2048., 2048.),
        );
        let ids: Vec<_> = (0..count).collect();
        let mut index = SelectionIndex::default();
        index.rebuild(&instructions, &ids, &s, |_| None);
        let queries: Vec<_> = (0..128)
            .map(|n| {
                s.world_to_screen(WorldPoint::new(
                    ((n * 17) % 64) as f64 + 0.3,
                    ((n * 29) % 64) as f64 + 0.3,
                ))
            })
            .collect();
        let hits = |q: ScreenPoint, subset: &[usize]| {
            subset
                .iter()
                .filter_map(|&id| {
                    hit_geometry(&instructions[id], &s, q, 3.)
                        .map(|hit| (id, hit.distance, hit.nearest))
                })
                .collect::<Vec<_>>()
        };
        let mut candidate_counts = Vec::new();
        for &q in &queries {
            let candidates = index.candidates(&s, q, 3., false);
            anyhow::ensure!(hits(q, &ids) == hits(q, &candidates), "candidate mismatch");
            candidate_counts.push(candidates.len());
        }
        let mut baseline = Vec::new();
        let mut indexed = Vec::new();
        for run in 0..5 {
            for indexed_run in if run % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let start = Instant::now();
                for &q in &queries {
                    if indexed_run {
                        let candidates = index.candidates(&s, q, 3., false);
                        std::hint::black_box(hits(q, &candidates));
                    } else {
                        std::hint::black_box(hits(q, &ids));
                    }
                }
                let seconds = start.elapsed().as_secs_f64();
                if indexed_run {
                    indexed.push(seconds);
                } else {
                    baseline.push(seconds);
                }
            }
        }
        rows.push(serde_json::json!({"synthetic_distribution":kind,"primitives":count,"queries":queries.len(),"oracle_equal":true,"baseline_seconds":baseline,"indexed_seconds":indexed,"candidate_counts":candidate_counts,"index":index.statistics(),"execution_os":std::env::consts::OS,"physical_input_verified":false}));
    }
    std::fs::write(output, serde_json::to_vec_pretty(&rows)?)?;
    Ok(())
}
