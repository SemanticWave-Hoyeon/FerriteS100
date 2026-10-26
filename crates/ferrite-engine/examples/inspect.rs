//! Open a folder (or dataset files) with the native platform and list the
//! datasets, their feature types and validation issues as text.
//!
//! ```text
//! cargo run -p ferrite-engine --example inspect -- [--catalogues DIR] [--verify] PATH...
//! ```
//!
//! `--catalogues` defaults to `Catalogues` in the current directory;
//! `--verify` requires valid signatures (trust anchor `Trust/IHO-S100-5.2.pem`
//! beside the catalogue folder, when present).
use anyhow::{bail, Context, Result};
use ferrite_engine::chart_engine::{
    ChartEngine, DatasetSource, EngineConfig, IssueSeverity, SignaturePolicy,
};
use ferrite_engine::platform::Platform;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut catalogues = PathBuf::from("Catalogues");
    let mut verify = false;
    let mut inputs = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--catalogues" => catalogues = args.next().context("--catalogues DIR")?.into(),
            "--verify" => verify = true,
            "-h" | "--help" => {
                println!("inspect [--catalogues DIR] [--verify] PATH...");
                return Ok(());
            }
            _ => inputs.push(PathBuf::from(arg)),
        }
    }
    if inputs.is_empty() {
        bail!("Usage: inspect [--catalogues DIR] [--verify] PATH...");
    }

    let mut config = EngineConfig::new(Platform::native(), &catalogues);
    if verify {
        config.signatures = SignaturePolicy::Required;
        let anchor = catalogues
            .parent()
            .unwrap_or(&catalogues)
            .join("Trust/IHO-S100-5.2.pem");
        if anchor.is_file() {
            config
                .trust_anchors
                .push(("IHO".into(), std::fs::read(&anchor)?));
        }
    }
    let mut engine = ChartEngine::new(config)?;
    for error in engine.catalogue_errors() {
        println!("note: {error}");
    }

    let (folders, files): (Vec<_>, Vec<_>) = inputs.into_iter().partition(|p| p.is_dir());
    let mut sources: Vec<_> = folders.into_iter().map(DatasetSource::Folder).collect();
    if !files.is_empty() {
        sources.push(DatasetSource::Files(files));
    }
    let mut issues = Vec::new();
    for source in sources {
        let job = engine.open(source);
        let report = engine.install(job.wait()?)?;
        for notice in &report.notices {
            println!("note: {notice}");
        }
        issues.extend(report.issues);
    }

    let datasets = engine.datasets();
    println!("{} dataset(s)", datasets.len());
    for dataset in &datasets {
        println!(
            "\n{} {} (edition {}, {} features, {})",
            dataset.product, dataset.name, dataset.edition, dataset.features, dataset.provenance
        );
        if let Some(source) = &dataset.source {
            println!("  source: {}", source.display());
        }
        for (feature_type, count) in engine.feature_types(dataset.id) {
            println!("  {count:6}  {feature_type}");
        }
    }

    let errors = issues
        .iter()
        .filter(|i| i.severity == IssueSeverity::Error)
        .count();
    println!(
        "\n{} validation issue(s): {} error(s), {} warning(s)",
        issues.len(),
        errors,
        issues.len() - errors
    );
    for issue in &issues {
        println!("  {issue}");
    }
    Ok(())
}
