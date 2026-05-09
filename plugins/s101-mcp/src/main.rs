//! S-101 MCP server entry point.
//!
//! Loads an S-101 cell + Feature Catalogue, builds in-memory indices, and
//! either runs `--validate` (Phase 0 backend self-check) or serves MCP over
//! stdio (Phase 1, default).
//!
//! Layout follows plan2.md §4 / §11. The split into Feature / Geometry /
//! Catalogue / Evidence indices keeps the catalogue-aware queries a single
//! pure function of (loaded data + query) — no external state.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_core::S101Cell;

// All shared modules (`indices`, `validate`, `mcp`) come from this crate's
// lib face (`lib.rs`) so other research tools and the FerriteS100 host can
// reuse the same types and tool dispatch.
use s101_mcp::{indices, mcp, validate};

#[derive(Debug, Parser)]
#[command(
    name = "s101-mcp",
    about = "Read-only MCP server for S-101 ENC QA research"
)]
struct Args {
    /// Path to the S-101 .000 chart cell to load.
    #[arg(long)]
    chart: PathBuf,

    /// Path to the Feature Catalogue XML (or directory containing it).
    #[arg(long)]
    catalogue: PathBuf,

    /// Run Phase 0 backend validation against the embedded sample list and exit
    /// with a structured report. Skips the MCP server.
    #[arg(long)]
    validate: bool,

    /// Log level for stderr (stdout is reserved for MCP JSON-RPC frames).
    #[arg(long, default_value = "info")]
    log: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(&args.log);

    tracing::info!("Loading S-101 cell: {}", args.chart.display());
    let mut cell = S101Cell::load(&args.chart)
        .with_context(|| format!("Failed to load chart {}", args.chart.display()))?;

    tracing::info!("Loading Feature Catalogue: {}", args.catalogue.display());
    let fc_path = resolve_fc_path(&args.catalogue)?;
    let fc = FeatureCatalogue::load(&fc_path)
        .with_context(|| format!("Failed to load catalogue {}", fc_path.display()))?;

    // Normalise dataset feature codes against FC codes — FerriteS100's main.rs
    // does this after loading; without it, dataset codes like "BuoyLateral"
    // never get rewritten to the FC's "LateralBuoy" form. Without this step
    // every prefix-swap-style mismatch shows up as a fake catalogue gap.
    cell.normalize_feature_codes(&fc.feature_type_codes());

    let indices = Arc::new(indices::Indices::build(cell, fc));
    tracing::info!(
        "Indices ready: {} features, {} catalogue feature types, {} simple attrs",
        indices.feature.len(),
        indices.catalogue.feature_count(),
        indices.catalogue.simple_attribute_count()
    );

    if args.validate {
        let report = validate::run(&indices);
        // Backend validation report goes to STDOUT as JSON so the caller can
        // parse it; logs (which go to stderr) describe progress.
        println!("{}", serde_json::to_string_pretty(&report)?);
        let pass_rate = report.pass_rate();
        if pass_rate < 0.95 {
            tracing::warn!(
                "Backend validation pass rate {:.2}% is below the plan2 §8.1 threshold (≥95%)",
                pass_rate * 100.0
            );
            std::process::exit(1);
        }
        tracing::info!(
            "Backend validation passed: {:.2}% (≥ 95% threshold)",
            pass_rate * 100.0
        );
        return Ok(());
    }

    tracing::info!("Starting MCP stdio server (read-only, plan2 §10 constraints)");
    mcp::server::run(indices)?;
    Ok(())
}

fn init_logging(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    // Stderr only — stdout is the MCP JSON-RPC channel.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("s101_mcp={level},info")));
    fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Accept either a direct XML file or a directory containing the FC XML.
fn resolve_fc_path(path: &PathBuf) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.clone());
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("Feature_Catalogue") && n.ends_with(".xml"))
            {
                return Ok(p);
            }
        }
        anyhow::bail!(
            "No *Feature_Catalogue*.xml found in directory {}",
            path.display()
        );
    }
    anyhow::bail!("Catalogue path does not exist: {}", path.display());
}
