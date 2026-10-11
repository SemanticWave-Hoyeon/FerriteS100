//! CPU lookup-only example: original independent oracle vs production candidate.
//! No GPU/window. Root must pin candidate source/executable receipts before running.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::{GridWindow, NumericCoverageSource};
use ferrite_lua::{DrawingCommand, LookupEntry};
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, ColorProfile, PortrayalCatalogue};
use ferrite_s102::{BathymetryCoverage, BathymetryPortrayal, DepthSettings};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    fs::File,
    hint::black_box,
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::Instant,
};
const MAX_SAMPLES: usize = 262_144;
const SOURCE_CAP: usize = 128 * 1024 * 1024;
const OUTPUT_CAP: usize = 4 * 1024 * 1024;
const QUARTETS: usize = 12;
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn json_string(s: &str) -> String {
    let mut result = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if (c as u32) < 32 => {
                write!(result, "\\u{:04x}", c as u32).unwrap();
            }
            c => result.push(c),
        }
    }
    result.push('"');
    result
}
fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut sha = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        sha.update(&buffer[..n]);
    }
    Ok(hex(&sha.finalize()))
}
fn values_hash(values: &[Option<f64>]) -> String {
    let mut sha = Sha256::new();
    for v in values {
        match v {
            Some(v) => {
                sha.update([1]);
                sha.update(v.to_bits().to_le_bytes());
            }
            None => sha.update([0]),
        }
    }
    hex(&sha.finalize())
}
fn rgba_hash(values: &[[u8; 4]]) -> String {
    let mut sha = Sha256::new();
    for v in values {
        sha.update(v);
    }
    hex(&sha.finalize())
}
fn old(entries: &[LookupEntry], profile: &ColorProfile, value: Option<f64>) -> Result<[u8; 4]> {
    let Some(depth) = value else {
        return Ok([0, 0, 0, 0]);
    };
    ensure!(depth.is_finite(), "Non-finite portrayal depth");
    let e = entries
        .iter()
        .find(|e| e.closure.contains(depth, e.range_min, e.range_max))
        .context("Depth outside PC lookup intervals")?;
    let rgb = profile
        .get_srgb(e.color_token.as_deref().unwrap())
        .context("Missing coverage colour")?;
    Ok([
        rgb.r,
        rgb.g,
        rgb.b,
        ((1. - e.transparency.clamp(0., 1.)) * 255.).round() as u8,
    ])
}
fn compare(a: Result<[u8; 4]>, b: Result<[u8; 4]>) -> Result<()> {
    match (a, b) {
        (Ok(a), Ok(b)) => ensure!(a == b, "RGBA differs"),
        (Err(a), Err(b)) => ensure!(a.to_string() == b.to_string(), "Error diagnostics differ"),
        _ => anyhow::bail!("Candidate/original acceptance differs"),
    }
    Ok(())
}
fn fill(
    method: bool,
    p: &BathymetryPortrayal,
    entries: &[LookupEntry],
    profile: &ColorProfile,
    values: &[Option<f64>],
    out: &mut [[u8; 4]],
) -> Result<()> {
    for (v, dest) in values.iter().zip(out.iter_mut()) {
        *dest = if method {
            p.rgba_value(black_box(*v))?
        } else {
            old(entries, profile, black_box(*v))?
        };
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn timed(
    method: bool,
    repeats: usize,
    p: &BathymetryPortrayal,
    entries: &[LookupEntry],
    profile: &ColorProfile,
    values: &[Option<f64>],
    out: &mut [[u8; 4]],
) -> Result<u128> {
    let start = Instant::now();
    for _ in 0..repeats {
        fill(method, p, entries, profile, values, out)?;
    }
    let _ = black_box(&*out);
    Ok(start.elapsed().as_nanos())
}
fn append(out: &mut String, text: &str) -> Result<()> {
    ensure!(
        out.len()
            .checked_add(text.len())
            .is_some_and(|n| n <= OUTPUT_CAP),
        "JSON output exceeds 4MiB"
    );
    out.push_str(text);
    Ok(())
}
fn decoded_values(path: Option<&Path>) -> Result<(Vec<Option<f64>>, String, Option<String>)> {
    if let Some(path) = path {
        let hash = file_hash(path)?;
        let started = Instant::now();
        // Public reader enforces its edition/metadata/quality/range checks. This is
        // not the authenticated App loading transaction or datum-policy composition.
        let coverages = BathymetryCoverage::open(path)?;
        let coverage = coverages.first().context("No S102 coverage instance")?;
        ensure!(
            !coverage.requires_spatial_mask(),
            "Geometric domain requires clipping; benchmark refuses flattening"
        );
        let g = coverage.numeric_geometry();
        let width = g.width.min(512);
        let height = g.height.min(MAX_SAMPLES / width.max(1));
        ensure!(width > 0 && height > 0, "Empty numeric grid");
        let mut selected = None;
        let candidates = [
            (0.5, 0.5),
            (0.25, 0.25),
            (0.75, 0.75),
            (0.25, 0.75),
            (0.75, 0.25),
            (0., 0.5),
            (1., 0.5),
            (0.5, 0.),
            (0.5, 1.),
        ];
        for (index, (fx, fy)) in candidates.into_iter().enumerate() {
            let window = GridWindow {
                column: ((g.width - width) as f64 * fx).round() as usize,
                row: ((g.height - height) as f64 * fy).round() as usize,
                width,
                height,
            };
            let mut values = Vec::with_capacity(width * height);
            coverage.visit_window_values(window, &mut |i, value| {
                ensure!(i == values.len(), "Visitor order differs");
                ensure!(value.is_none_or(f64::is_finite), "Nonfinite decoded source");
                values.push(value);
                Ok(())
            })?;
            ensure!(
                values.len() == width * height,
                "Visitor sample count differs"
            );
            let non_null = values.iter().filter(|v| v.is_some()).count();
            eprintln!(
                "Numeric window discovery: grid={}x{}, window={},{},{}x{}, nonnull={}",
                g.width, g.height, window.column, window.row, width, height, non_null
            );
            if non_null >= (values.len() / 8).max(101) {
                selected = Some((window, values, index + 1));
                break;
            }
        }
        let (window, values, candidates_read) = selected.context(
            "No meaningful bounded real numeric window; refuse timing missing-only values",
        )?;
        let column = window.column;
        let row = window.row;
        let elapsed = started.elapsed().as_nanos();
        ensure!(file_hash(path)? == hash, "HDF file changed during read");
        let metadata=format!("{{\"kind\":\"real_public_reader_first_instance_window\",\"path\":{},\"sha256\":{},\"instance\":{},\"product_specification\":{},\"window\":[{column},{row},{width},{height}],\"bounded_window_candidates_read\":{candidates_read},\"window_selection\":\"first fixed candidate with at least one eighth nonnull\",\"grid_f64_bits\":[{},{},{},{}],\"vertical_datum\":{},\"source_decode_ns_descriptive\":{elapsed},\"authenticated_app_load\":false,\"datum_policy_composition\":false}}",json_string(&path.display().to_string()),json_string(&hash),json_string(&coverage.instance_name),json_string(&coverage.product_specification),g.origin_x.to_bits(),g.origin_y.to_bits(),g.spacing_x.to_bits(),g.spacing_y.to_bits(),coverage.vertical_datum);
        Ok((values, metadata, Some(hash)))
    } else {
        let mut values = Vec::with_capacity(MAX_SAMPLES);
        let special = [
            None,
            Some(-0.),
            Some(0.),
            Some(2. - 1e-8),
            Some(2.),
            Some(2. + 1e-8),
            Some(10. - 1e-8),
            Some(10.),
            Some(10. + 1e-8),
            Some(30. - 1e-8),
            Some(30.),
            Some(30. + 1e-8),
        ];
        for i in 0..MAX_SAMPLES {
            values.push(if i % 29 == 0 {
                special[(i / 29) % special.len()]
            } else {
                Some(((i * 37) % 1024) as f64 / 10. - 10.)
            });
        }
        Ok((values,"{\"kind\":\"synthetic_fixed_depth_sweep_with_null_and_boundaries\",\"actual_dataset\":false,\"seed_rule\":\"(i*37)%1024/10-10; special every29\"}".into(),None))
    }
}
fn run() -> Result<()> {
    let production_source = include_str!("../src/portrayal.rs");
    ensure!(
        production_source.contains("struct ResolvedDepthEntry")
            && production_source.contains("Ok(entry.rgba)"),
        "Production candidate is absent from compiled source"
    );
    let args: Vec<_> = std::env::args_os().collect();
    ensure!(
        args.len() == 2 || args.len() == 3,
        "Usage: s102_color_lookup_cpu PC_DIRECTORY [S102_H5]"
    );
    // Root must supply these from its guarded fresh-build/source receipts.
    let source_receipt = std::env::var("FERRITE_BENCH_SOURCE_MANIFEST_SHA256")
        .context("Missing source receipt SHA")?;
    let expected_exe =
        std::env::var("FERRITE_BENCH_EXE_SHA256").context("Missing executable receipt SHA")?;
    for digest in [&source_receipt, &expected_exe] {
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid receipt SHA"
        );
    }
    let exe = std::env::current_exe()?;
    let exe_sha = file_hash(&exe)?;
    ensure!(
        exe_sha == expected_exe.to_ascii_lowercase(),
        "Executable receipt differs"
    );
    let pc_path = Path::new(&args[1]);
    let pc: Arc<BoundPortrayalCatalogue> = Arc::new(PortrayalCatalogue::load_bound(pc_path)?);
    let pc_sha = hex(pc.source_digest());
    let rule_sha = hex(&Sha256::digest(
        pc.sources()
            .read_relative(Path::new("Rules/BathymetryCoverage.lua"))?,
    ));
    let mut profiles: Vec<_> = pc.color_profiles.profiles.keys().cloned().collect();
    profiles.sort();
    ensure!(
        profiles.len() == 3,
        "This bounded 24-case recipe requires exactly 3 captured profiles"
    );
    let hdf = args.get(2).map(Path::new);
    let (values, source_metadata, source_hdf_hash) = decoded_values(hdf)?;
    let hdf_stat = hdf
        .map(|p| {
            let m = std::fs::metadata(p)?;
            Ok::<_, std::io::Error>((m.len(), m.modified()?))
        })
        .transpose()?;
    ensure!(
        !values.is_empty() && values.len() <= MAX_SAMPLES,
        "Invalid sample count"
    );
    let source_payload = values
        .len()
        .checked_mul(std::mem::size_of::<Option<f64>>() + 8)
        .context("Payload overflow")?;
    ensure!(
        source_payload <= SOURCE_CAP,
        "Decoded values+two RGBA outputs exceed 128MiB"
    );
    let non_null = values.iter().filter(|v| v.is_some()).count();
    let values_sha = values_hash(&values);
    let mut a = vec![[0; 4]; values.len()];
    let mut b = vec![[0; 4]; values.len()];
    let mut output = String::new();
    let whole_started = Instant::now();
    let mut case = 0;
    for profile_id in &profiles {
        for four_shades in [false, true] {
            for (shallow, safety, deep) in
                [(2., 30., 30.), (2., 10., 30.), (0., 0., 0.), (2., 2., 2.)]
            {
                ensure!(
                    whole_started.elapsed().as_secs() < 300,
                    "Benchmark exceeded bounded 300s case-start limit"
                );
                let settings = DepthSettings {
                    shallow_contour: shallow,
                    safety_contour: safety,
                    deep_contour: deep,
                    four_shades,
                };
                let started = Instant::now();
                let p = BathymetryPortrayal::from_bound_catalogue(
                    Arc::clone(&pc),
                    profile_id,
                    settings,
                )?;
                let construction = started.elapsed().as_nanos();
                let owner = p.bound_evaluation().context("Missing actual bound owner")?;
                ensure!(
                    Arc::ptr_eq(owner.catalogue(), &pc) && owner.profile_id() == profile_id,
                    "Actual PC/profile owner differs"
                );
                let bound_settings = owner.settings();
                ensure!(
                    bound_settings.shallow_contour.to_bits() == shallow.to_bits()
                        && bound_settings.safety_contour.to_bits() == safety.to_bits()
                        && bound_settings.deep_contour.to_bits() == deep.to_bits()
                        && bound_settings.four_shades == four_shades,
                    "Actual bound settings differ"
                );
                let actual_rule = pc
                    .sources()
                    .read_relative(Path::new("Rules/BathymetryCoverage.lua"))?;
                ensure!(
                    owner.executed_rule() == actual_rule.as_ref(),
                    "Actual executed rule differs"
                );
                let parsed =
                    ferrite_lua::parse_instruction_string("BathymetryCoverage", &p.instructions)?;
                let entries = parsed
                    .commands
                    .into_iter()
                    .find_map(|c| match c {
                        DrawingCommand::CoverageFill {
                            attribute_code,
                            lookup_entries,
                            ..
                        } if attribute_code == "depth" => Some(lookup_entries),
                        _ => None,
                    })
                    .context("No original CoverageFill entries")?;
                ensure!(
                    !entries.is_empty() && entries.len() <= 4096,
                    "Lookup entry budget"
                );
                let profile = &pc.color_profiles.profiles[profile_id];
                for value in values.iter().copied().chain([
                    None,
                    Some(f64::NAN),
                    Some(f64::INFINITY),
                    Some(f64::NEG_INFINITY),
                    Some(-0.),
                    Some(f64::from_bits(1)),
                    Some(f64::MAX),
                    Some(-f64::MAX),
                    Some(shallow - 1e-8),
                    Some(shallow),
                    Some(shallow + 1e-8),
                    Some(safety - 1e-8),
                    Some(safety),
                    Some(safety + 1e-8),
                    Some(deep - 1e-8),
                    Some(deep),
                    Some(deep + 1e-8),
                ]) {
                    compare(old(&entries, profile, value), p.rgba_value(value))?;
                }
                fill(false, &p, &entries, profile, &values, &mut a)?;
                fill(true, &p, &entries, profile, &values, &mut b)?;
                ensure!(a == b, "Pre-timing full pixel mismatch");
                let pixels_sha = rgba_hash(&a);
                for method in [false, true, true, false, false, true, true, false] {
                    if method {
                        fill(true, &p, &entries, profile, &values, &mut b)?;
                    } else {
                        fill(false, &p, &entries, profile, &values, &mut a)?;
                    }
                }
                let mut repeats = 1usize;
                loop {
                    let ta = timed(false, repeats, &p, &entries, profile, &values, &mut a)?;
                    let tb = timed(true, repeats, &p, &entries, profile, &values, &mut b)?;
                    if ta.min(tb) >= 20_000_000 || repeats == 64 {
                        break;
                    }
                    repeats *= 2;
                }
                let mut rows = String::new();
                for quartet in 0..QUARTETS {
                    ensure!(
                        whole_started.elapsed().as_secs() < 300,
                        "Benchmark exceeded bounded 300s quartet-start limit"
                    );
                    let sequence = if quartet % 2 == 0 {
                        [false, true, true, false]
                    } else {
                        [true, false, false, true]
                    };
                    for (leg, method) in sequence.into_iter().enumerate() {
                        let elapsed = if method {
                            timed(true, repeats, &p, &entries, profile, &values, &mut b)?
                        } else {
                            timed(false, repeats, &p, &entries, profile, &values, &mut a)?
                        };
                        if !rows.is_empty() {
                            rows.push(',');
                        }
                        write!(rows,"{{\"quartet\":{quartet},\"sequence\":\"{}\",\"leg\":{leg},\"method\":\"{}\",\"wall_ns\":{elapsed}}}",if quartet%2==0{"ABBA"}else{"BAAB"},if method{"candidate"}else{"original"})?;
                    }
                    ensure!(a == b, "Post-quartet full pixel mismatch");
                    if let (Some(path), Some(stat)) = (hdf, hdf_stat.as_ref()) {
                        let m = std::fs::metadata(path)?;
                        ensure!(
                            (m.len(), m.modified()?) == *stat,
                            "Real source metadata changed between quartets"
                        );
                    }
                    ensure!(
                        hex(PortrayalCatalogue::load_bound(pc_path)?.source_digest()) == pc_sha,
                        "PC source changed between quartets"
                    );
                }
                let case_json=format!("{{\"case\":{case},\"scope\":\"CPU_lookup_only_not_end_to_end_FPS_min_20ms_calibration\",\"source_receipt_sha256\":{},\"executable_sha256\":{},\"pc_source_sha256\":{},\"rule_sha256\":{},\"profile\":{},\"contours_f64_bits\":[{},{},{}],\"four_shades\":{four_shades},\"non_null_pixels\":{non_null},\"pixels_per_repeat\":{},\"repeats\":{repeats},\"construction_ns_descriptive\":{construction},\"lookup_entries\":{},\"decoded_payload_plus_outputs_bytes\":{source_payload},\"source\":{source_metadata},\"input_values_sha256\":{},\"output_rgba_sha256\":{},\"every_pixel_error_preverified\":true,\"all_quartets_exact_rgba\":true,\"bound_pc_arc_verified\":true,\"rows\":[{rows}]}}\n",json_string(&source_receipt),json_string(&exe_sha),json_string(&pc_sha),json_string(&rule_sha),json_string(profile_id),shallow.to_bits(),safety.to_bits(),deep.to_bits(),values.len(),entries.len(),json_string(&values_sha),json_string(&pixels_sha));
                append(&mut output, &case_json)?;
                case += 1;
            }
        }
    }
    ensure!(case == 24, "Case count differs");
    ensure!(file_hash(&exe)? == exe_sha, "Executable changed");
    if let (Some(path), Some(hash)) = (hdf, source_hdf_hash.as_ref()) {
        ensure!(
            file_hash(path)? == *hash,
            "Real source content changed before output"
        );
    }
    std::io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("Lookup benchmark refused: {error:#}");
        std::process::exit(1);
    }
}
