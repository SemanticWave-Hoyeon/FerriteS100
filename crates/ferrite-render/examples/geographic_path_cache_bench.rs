//! Synthetic retained standard geometry, CPU-only. No actual PC, GPU or FPS claim.
use anyhow::{ensure, Context, Result};
use ferrite_render::{
    DrawingInstruction, FlatProjection, GeoBounds, LineInstruction, PortrayalPath, RenderContext,
    RetainedPathCacheStats, Scaler, ScreenPoint, Viewport, WorldPoint, WrappedGeometryHit,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    hint::black_box,
    io::{Read, Write},
    path::Path,
    time::Instant,
};
const ITERATIONS: usize = 16;
const QUARTETS: usize = 12;
const OUTPUT_CAP: usize = 4 * 1024 * 1024;
#[derive(Clone, Copy)]
struct Case {
    shape: usize,
    environment: usize,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut sha = Sha256::new();
    let mut b = [0u8; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        sha.update(&b[..n]);
    }
    Ok(hex(&sha.finalize()))
}
fn stats(s: RetainedPathCacheStats) -> Value {
    json!({"hits":s.hits,"misses":s.misses,"declines":s.declines,"retained_payload_bytes":s.retained_payload_bytes,"entries":s.entries})
}
fn environment(index: usize) -> ((f64, f64), FlatProjection) {
    match index {
        0 => ((179.9, 50.), FlatProjection::LocalGeographic),
        1 => ((15., 80.), FlatProjection::EllipsoidalMercator),
        2 => ((179.9, 50.), FlatProjection::EllipsoidalMercator),
        _ => ((15., 80.), FlatProjection::LocalGeographic),
    }
}
fn fixture(case: Case, enabled: bool) -> RenderContext {
    let ((lon, lat), projection) = environment(case.environment);
    let mut c = RenderContext::new(Viewport::new(800., 600.));
    c.set_retained_path_cache_enabled(enabled);
    c.set_bounds(GeoBounds::new(
        lon - 0.35,
        lat - 0.35,
        lon + 0.35,
        lat + 0.35,
    ));
    c.scaler.set_projection(projection);
    let p = WorldPoint::new(lon, lat);
    let mut line = LineInstruction::new(vec![p, p]);
    if case.shape < 8 {
        let sweep = match case.shape % 4 {
            0 => 360.,
            1 => -360.,
            2 => 270.,
            _ => -270.,
        };
        line.portrayal_path = Some(if case.shape < 4 {
            PortrayalPath::GeographicArc {
                center: (lon, lat),
                radius_m: 2000.,
                start: 35.,
                sweep,
            }
        } else {
            PortrayalPath::GeographicAnnulus {
                center: (lon, lat),
                outer: 2000.,
                inner: 900.,
                start: 35.,
                sweep,
            }
        });
    } else {
        line.points = vec![
            WorldPoint::new(lon - 0.1, lat - 0.1),
            WorldPoint::new(lon, lat),
            WorldPoint::new(lon + 0.1, lat + 0.1),
        ];
    }
    line.style.dash_pattern = vec![8., 5.];
    c.add_instruction(DrawingInstruction::Line(line));
    c
}
fn frames(case: Case) -> Vec<Scaler> {
    let c = fixture(case, false);
    let ((lon, lat), _) = environment(case.environment);
    [
        (0., 0., 1., 1.),
        (0.003, 0., 0.9, 1.),
        (0., 0.002, 1.1, 2.),
        (-0.002, -0.001, 1.0, 1.),
    ]
    .into_iter()
    .map(|(dx, dy, factor, dpi)| {
        let mut s = c.scaler.clone();
        s.set_bounds(GeoBounds::new(
            lon + dx - 0.35 * factor,
            lat + dy - 0.35 * factor,
            lon + dx + 0.35 * factor,
            lat + dy + 0.35 * factor,
        ));
        s.set_pixel_ratio(dpi);
        s
    })
    .collect()
}
fn original_runs(c: &RenderContext) -> Vec<Vec<WorldPoint>> {
    let DrawingInstruction::Line(line) = &c.raw_instructions()[0] else {
        unreachable!()
    };
    line.render_paths(&c.scaler)
        .map(|p| p.into_owned())
        .collect()
}
fn bits(runs: &[Vec<WorldPoint>]) -> Vec<Vec<[u64; 2]>> {
    runs.iter()
        .map(|r| r.iter().map(|p| [p.x.to_bits(), p.y.to_bits()]).collect())
        .collect()
}
fn hit_bits(hit: Option<WrappedGeometryHit>) -> Option<[u64; 4]> {
    hit.map(|h| {
        [
            h.hit.distance.to_bits(),
            u64::from(h.hit.nearest.x.to_bits()),
            u64::from(h.hit.nearest.y.to_bits()),
            h.longitude_shift.to_bits(),
        ]
    })
}
fn qualify(
    off: &mut RenderContext,
    on: &mut RenderContext,
    frames: &[Scaler],
) -> Result<(Vec<ScreenPoint>, Vec<Value>)> {
    ensure!(
        bincode::serialize(off.raw_instructions())? == bincode::serialize(on.raw_instructions())?,
        "A/B original IR differs"
    );
    let mut queries = Vec::new();
    let mut receipts = Vec::new();
    for frame in frames {
        off.scaler = frame.clone();
        on.scaler = frame.clone();
        let original = original_runs(off);
        let expected = bits(&original);
        let off_runs = off
            .resolved_line_paths(0, &off.scaler)
            .context("Off line missing")?
            .map(|p| p.into_owned())
            .collect::<Vec<_>>();
        let on_runs = on
            .resolved_line_paths(0, &on.scaler)
            .context("On line missing")?
            .map(|p| p.into_owned())
            .collect::<Vec<_>>();
        ensure!(
            expected == bits(&off_runs) && expected == bits(&on_runs),
            "Original/Off/On coordinate or run structure differs"
        );
        let first = original
            .first()
            .and_then(|r| r.first())
            .context("Synthetic scene unexpectedly empty")?;
        let q = off.scaler.world_to_screen(*first);
        ensure!(q.x.is_finite() && q.y.is_finite(), "Invalid query");
        queries.push(q);
        let reference = ferrite_render::hit_geometry_wrapped_visible(
            &off.raw_instructions()[0],
            &off.scaler,
            q,
            3.,
            None,
            true,
        );
        ensure!(
            reference.is_some(),
            "Synthetic first dash has no nonvacuous hit"
        );
        for dx in [-2., 0., 2.] {
            let q = ScreenPoint::new(q.x + dx, q.y);
            let expected = hit_bits(ferrite_render::hit_geometry_wrapped_visible(
                &off.raw_instructions()[0],
                &off.scaler,
                q,
                3.,
                None,
                true,
            ));
            ensure!(
                expected
                    == hit_bits(ferrite_render::hit_geometry_wrapped_visible_in_context(
                        off, 0, q, 3., None, true
                    )),
                "Off pick differs"
            );
            ensure!(
                expected
                    == hit_bits(ferrite_render::hit_geometry_wrapped_visible_in_context(
                        on, 0, q, 3., None, true
                    )),
                "On pick/dash/wrapping differs"
            );
        }
        let serialized = bincode::serialize(&expected)?;
        let coordinate_sha = hex(&Sha256::digest(serialized));
        receipts.push(json!({"camera":frame.flat_encoded_identity(),"coordinate_f64_run_bits_sha256":coordinate_sha,"runs":original.len(),"points":original.iter().map(Vec::len).sum::<usize>(),"query":[q.x,q.y],"nonvacuous_hit":true}));
    }
    Ok((queries, receipts))
}
fn work(
    c: &mut RenderContext,
    frames: &[Scaler],
    queries: &[ScreenPoint],
    changing: bool,
) -> Result<u64> {
    let mut checksum = 0u64;
    for i in 0..ITERATIONS {
        let frame = if changing { i % frames.len() } else { 0 };
        c.scaler = frames[frame].clone();
        for run in c
            .resolved_line_paths(0, &c.scaler)
            .context("Line missing")?
        {
            for p in run.iter() {
                checksum = checksum.rotate_left(3) ^ black_box(p.x.to_bits()) ^ p.y.to_bits();
            }
        }
        let hit = ferrite_render::hit_geometry_wrapped_visible_in_context(
            c,
            0,
            queries[frame],
            3.,
            None,
            true,
        )
        .context("Nonvacuous timed hit disappeared")?;
        checksum ^= black_box(hit.hit.distance.to_bits());
        checksum ^= u64::from(hit.hit.nearest.x.to_bits());
    }
    Ok(black_box(checksum))
}
fn prime(
    c: &mut RenderContext,
    frames: &[Scaler],
    queries: &[ScreenPoint],
    changing: bool,
) -> Result<()> {
    let index = if changing { frames.len() - 1 } else { 0 };
    c.scaler = frames[index].clone();
    let _ = black_box(
        c.resolved_line_paths(0, &c.scaler)
            .context("Prime line missing")?
            .map(|p| p.len())
            .sum::<usize>(),
    );
    let _ = black_box(ferrite_render::hit_geometry_wrapped_visible_in_context(
        c,
        0,
        queries[index],
        3.,
        None,
        true,
    ));
    Ok(())
}
fn run() -> Result<()> {
    ensure!(
        include_str!("../src/retained_path_cache.rs").contains("RetainedPathCache"),
        "Compiled candidate absent"
    );
    let source_sha = std::env::var("FERRITE_BENCH_SOURCE_MANIFEST_SHA256")
        .context("Fresh source receipt required")?;
    let expected_exe = std::env::var("FERRITE_BENCH_EXE_SHA256")
        .context("Fresh guarded executable receipt required")?;
    for s in [&source_sha, &expected_exe] {
        ensure!(
            s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid SHA receipt"
        );
    }
    let executable = std::env::current_exe()?;
    let exe_sha = hash_file(&executable)?;
    ensure!(
        exe_sha == expected_exe.to_ascii_lowercase(),
        "Executable differs"
    );
    let mut cases = Vec::new();
    for environment in [0, 1] {
        for shape in 0..8 {
            cases.push(Case { shape, environment });
        }
    }
    for (shape, environment) in [
        (0, 2),
        (3, 2),
        (4, 2),
        (7, 2),
        (2, 3),
        (7, 3),
        (8, 0),
        (8, 1),
    ] {
        cases.push(Case { shape, environment });
    }
    ensure!(cases.len() == 24, "Wrong case matrix");
    let mut output = Vec::new();
    let whole_start = Instant::now();
    for (id, case) in cases.into_iter().enumerate() {
        let mut off = fixture(case, false);
        let mut on = fixture(case, true);
        let camera_frames = frames(case);
        let (queries, receipts) = qualify(&mut off, &mut on, &camera_frames)?;
        let original_ir_sha = hex(&Sha256::digest(bincode::serialize(off.raw_instructions())?));
        let mut rows = Vec::new();
        for changing in [false, true] {
            // Balanced discarded warmup, same work and inputs; no parsing/IO in timed loop.
            for enabled in [false, true, true, false] {
                let c = if enabled { &mut on } else { &mut off };
                prime(c, &camera_frames, &queries, changing)?;
                let _ = work(c, &camera_frames, &queries, changing)?;
            }
            for quartet in 0..QUARTETS {
                let mut checksums = [0u64; 4];
                for (leg, enabled) in [false, true, true, false].into_iter().enumerate() {
                    ensure!(
                        whole_start.elapsed().as_secs() < 180,
                        "Bounded benchmark exceeded case/leg-start180s limit"
                    );
                    let c = if enabled { &mut on } else { &mut off };
                    prime(c, &camera_frames, &queries, changing)?;
                    let before = c.retained_path_cache_stats();
                    let timer = Instant::now();
                    let checksum = work(c, &camera_frames, &queries, changing)?;
                    let wall_ns = timer.elapsed().as_nanos();
                    let after = c.retained_path_cache_stats();
                    checksums[leg] = checksum;
                    ensure!(
                        after.retained_payload_bytes <= 8 * 1024 * 1024 && after.entries <= 4096,
                        "Retained cap exceeded"
                    );
                    rows.push(json!({"phase":if changing{"camera_changing"}else{"warm_same_camera"},"quartet":quartet,"sequence":"OFF_ON_ON_OFF","leg":leg,"cache":enabled,"iterations":ITERATIONS,"wall_ns":wall_ns,"checksum":checksum,"before":stats(before),"after":stats(after),"hits_delta":after.hits-before.hits,"misses_delta":after.misses-before.misses}));
                }
                ensure!(
                    checksums.iter().all(|v| *v == checksums[0]),
                    "Timed traversal/pick checksum differs"
                );
                // Direct full original/candidate coordinate and pick bit validation outside timer.
                let _ = qualify(&mut off, &mut on, &camera_frames)?;
            }
        }
        let (center, projection) = environment(case.environment);
        let encoded = serde_json::to_vec(
            &json!({"case":id,"scope":"synthetic_standard_geometry_CPU_resolver_plus_exact_pick; no_actual_PC_GPU_FPS","shape":case.shape,"ordinary_control":case.shape==8,"center":center,"projection":format!("{projection:?}"),"source_receipt_sha256":source_sha,"executable_sha256":exe_sha,"original_ir_sha256":original_ir_sha,"frames":receipts,"original_f64_runs_and_hit_bits_verified":true,"quartets_per_phase":QUARTETS,"rows":rows}),
        )?;
        ensure!(
            output
                .len()
                .checked_add(encoded.len() + 1)
                .is_some_and(|n| n <= OUTPUT_CAP),
            "Output exceeds4MiB"
        );
        output.extend(encoded);
        output.push(b'\n');
    }
    ensure!(hash_file(&executable)? == exe_sha, "Executable changed");
    std::io::stdout().lock().write_all(&output)?;
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("Geographic CPU benchmark refused: {error:#}");
        std::process::exit(1);
    }
}
