//! Golden-image regression tests for the rendering pipeline.
//!
//! Each test launches the `ferrite-s100` binary with `--chart`, `--zoom`, and
//! `--screenshot`, then compares the produced PNG against a baseline committed
//! at `tests/goldens/<name>.png`.
//!
//! Why integration via subprocess instead of in-process: the renderer requires
//! a real winit window + wgpu adapter. Spawning the binary mirrors how users
//! actually run the app, and avoids reorganizing internal APIs purely for tests.
//!
//! Default `cargo test` does NOT run these — they need a working GPU and a
//! windowed environment. Opt in:
//!
//! ```text
//! FERRITE_GOLDEN_TESTS=1 cargo test --test golden_image --release
//! ```
//!
//! To regenerate baselines after an intentional rendering change:
//!
//! ```text
//! FERRITE_GOLDEN_TESTS=update cargo test --test golden_image --release
//! ```
//!
//! Tolerances allow minor GPU-driver / antialiasing variation. A real
//! regression — e.g. the red-square fallback bug that motivated this suite —
//! produces thousands of pixels with large channel deltas and is caught
//! immediately.

use std::path::{Path, PathBuf};
use std::process::Command;

use image::GenericImageView;

// Tolerances are intentionally tight: same-machine renders are byte-identical
// (verified via SHA-256), so any mismatch indicates a real change. The slack
// below exists only to absorb the modest AA / driver variation we expect when
// the suite is later run on different GPUs.

/// Per-channel max delta tolerated before a pixel is flagged as an outlier.
/// 0-255. A 12 is well below the magnitude of a real regression
/// (a red-square fallback gives channel deltas around 150-255).
const MAX_PIXEL_CHANNEL_DIFF: u8 = 12;

/// Maximum fraction of pixels allowed to exceed `MAX_PIXEL_CHANNEL_DIFF`.
/// 0.0005 = 1 outlier per 2000 pixels, ≈1000 pixels on a 1920×1080 frame —
/// roughly the footprint of a single misrendered symbol, which is exactly
/// the smallest regression we want to catch.
const MAX_OUTLIER_FRACTION: f64 = 0.0005;

/// Maximum mean channel-difference across the whole image. Diffuse regressions
/// (wrong color profile applied everywhere) will easily exceed 0.3.
const MAX_MEAN_DIFF: f64 = 0.3;

#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Skip the test entirely (default for `cargo test`).
    Skip,
    /// Compare actual against golden, fail on tolerance breach.
    Verify,
    /// Overwrite the golden with the actual output. Test passes
    /// unconditionally; intended for intentional baseline updates.
    Update,
}

fn run_mode() -> Mode {
    match std::env::var("FERRITE_GOLDEN_TESTS").as_deref() {
        Ok("update") => Mode::Update,
        Ok(v) if !v.is_empty() && v != "0" => Mode::Verify,
        _ => Mode::Skip,
    }
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn goldens_dir() -> PathBuf {
    project_root().join("tests").join("goldens")
}

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ferrite-s100"))
}

/// Render a chart by spawning the ferrite-s100 binary. Returns the path to
/// the produced PNG inside a per-test temp directory.
fn render_to_png(case_name: &str, chart_relative: &str, zoom: f64) -> PathBuf {
    let root = project_root();
    let chart = root.join(chart_relative);
    assert!(
        chart.exists(),
        "chart fixture missing: {} (golden tests need ChartData/ in the repo)",
        chart.display()
    );

    let out_dir = std::env::temp_dir().join(format!("ferrite_golden_{}", case_name));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).expect("create temp dir");
    let out_png = out_dir.join("rendered.png");

    let status = Command::new(binary_path())
        // CWD = project root so Catalogues/ resolves; this also exercises
        // the CWD-fallback branch of get_app_base_dir().
        .current_dir(&root)
        .arg("--chart")
        .arg(&chart)
        .arg("--screenshot")
        .arg(&out_png)
        .arg("--zoom")
        .arg(zoom.to_string())
        .status()
        .expect("spawn ferrite-s100");

    assert!(
        status.success(),
        "ferrite-s100 exited non-zero (status: {:?}) — log: {}",
        status,
        root.join("logs/ferrite_debug.log").display()
    );
    assert!(
        out_png.exists(),
        "ferrite-s100 produced no screenshot at {}",
        out_png.display()
    );
    out_png
}

/// Compare two PNGs with tolerance. Panics with a structured message on mismatch.
fn assert_images_match(actual: &Path, golden: &Path) {
    let a = image::open(actual)
        .unwrap_or_else(|e| panic!("failed to open actual {}: {}", actual.display(), e))
        .to_rgba8();
    let g = image::open(golden)
        .unwrap_or_else(|e| panic!("failed to open golden {}: {}", golden.display(), e))
        .to_rgba8();

    assert_eq!(
        (a.width(), a.height()),
        (g.width(), g.height()),
        "image dimensions differ: actual {}x{}, golden {}x{} ({})",
        a.width(),
        a.height(),
        g.width(),
        g.height(),
        actual.display()
    );

    let mut total_diff: u64 = 0;
    let mut outliers: u64 = 0;
    let total_channels = (a.width() as u64) * (a.height() as u64) * 4;

    for (px_a, px_g) in a.pixels().zip(g.pixels()) {
        let mut max_chan_diff = 0u8;
        for c in 0..4 {
            let d = px_a.0[c].abs_diff(px_g.0[c]);
            total_diff += d as u64;
            if d > max_chan_diff {
                max_chan_diff = d;
            }
        }
        if max_chan_diff > MAX_PIXEL_CHANNEL_DIFF {
            outliers += 1;
        }
    }

    let total_pixels = (a.width() as u64) * (a.height() as u64);
    let outlier_frac = outliers as f64 / total_pixels as f64;
    let mean_diff = total_diff as f64 / total_channels as f64;

    let passed = outlier_frac <= MAX_OUTLIER_FRACTION && mean_diff <= MAX_MEAN_DIFF;

    if !passed {
        // Persist the actual next to the golden for inspection.
        let debug_path = golden.with_extension("actual.png");
        let _ = std::fs::copy(actual, &debug_path);
        panic!(
            "golden image mismatch:\n  outlier pixels: {} / {} ({:.4}%, limit {:.4}%)\n  \
             mean channel diff: {:.3} (limit {:.3})\n  golden:  {}\n  actual:  {}\n  \
             To accept this change as the new baseline, rerun with FERRITE_GOLDEN_TESTS=update.",
            outliers,
            total_pixels,
            outlier_frac * 100.0,
            MAX_OUTLIER_FRACTION * 100.0,
            mean_diff,
            MAX_MEAN_DIFF,
            golden.display(),
            debug_path.display(),
        );
    }
}

fn run_case(case_name: &str, chart_relative: &str, zoom: f64) {
    let mode = run_mode();
    if matches!(mode, Mode::Skip) {
        eprintln!(
            "skipping golden test {} (set FERRITE_GOLDEN_TESTS=1 to enable)",
            case_name
        );
        return;
    }

    let actual = render_to_png(case_name, chart_relative, zoom);
    let golden = goldens_dir().join(format!("{}.png", case_name));

    match mode {
        Mode::Update => {
            std::fs::create_dir_all(goldens_dir()).expect("create goldens dir");
            std::fs::copy(&actual, &golden).expect("copy actual to golden");
            eprintln!("updated golden: {}", golden.display());
        }
        Mode::Verify => {
            assert!(
                golden.exists(),
                "golden missing for case '{}' at {}\nRun once with FERRITE_GOLDEN_TESTS=update to create it.",
                case_name,
                golden.display()
            );
            assert_images_match(&actual, &golden);
        }
        Mode::Skip => unreachable!(),
    }
}

// =============================================================================
// Cases
// =============================================================================

#[test]
fn chart_038_zoom_20() {
    run_case("chart_038_zoom_20", "ChartData/101GB00502038.000", 20.0);
}

#[test]
fn chart_038_zoom_5() {
    run_case("chart_038_zoom_5", "ChartData/101GB00502038.000", 5.0);
}
