//! `ferrite`: headless S-100 chart tools on the `ChartEngine` facade.
//!
//! ```text
//! ferrite inspect  [OPTIONS] PATH...
//! ferrite validate [OPTIONS] PATH...
//! ferrite render   [OPTIONS] PATH... --out chart.png [VIEW]
//! ferrite precompute [OPTIONS] PATH... --cache-dir DIR [VIEW]
//! ```
//! See `ferrite --help`.
use anyhow::{bail, ensure, Context, Result};
use ferrite_cli::{
    engine_config, inspection_json, issue_json, open_all, palettes, save_png, sources_for_paths,
    OffscreenChartRenderer,
};
use ferrite_engine::chart_engine::{
    Camera, ChartEngine, DisplayOptions, IssueSeverity, SignaturePolicy, View, Zoom,
};
use ferrite_engine::platform::{DirectoryCacheStore, Platform};
use ferrite_engine::{PortrayalSource, ViewportPixels};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
ferrite - headless S-100 chart tools

USAGE:
    ferrite inspect    [OPTIONS] PATH...
    ferrite validate   [OPTIONS] PATH...
    ferrite render     [OPTIONS] PATH... --out FILE.png [VIEW]
    ferrite precompute [OPTIONS] PATH... --cache-dir DIR [VIEW]

PATH is a dataset folder (exchange sets included) or dataset files.

OPTIONS:
    --catalogues DIR    Catalogue folder with FC/ and PC/ (default: ./Catalogues
                        or $FERRITE_CATALOGUES)
    --inventory DIR     Installed S-101 catalogue inventory for other editions
    --verify            Require valid dataset signatures (trust anchor
                        Trust/IHO-S100-5.2.pem beside the catalogue folder)
    --json              Machine-readable output (inspect, validate)
    -v, --verbose       Log progress to stderr (RUST_LOG overrides)

VIEW (render, precompute):
    --center LAT,LON    View centre (default: centre of the data)
    --zoom Z            Zoom over the all-data view (default 1)
    --scale N           Display scale 1:N instead of --zoom
    --size WxH          Image size in pixels (default 1920x1018)
    --density F         Physical pixels per logical pixel (default 1)
    --palette NAME      Colour profile: Day, Dusk or Night (default Day)
    --precomputed DIR   render: portray S-101 cells from precomputed payloads
                        in DIR instead of running the Lua rules

validate exits with status 1 when any error is found, 2 on failures.
precompute writes the per-cell portrayal payloads of the view to DIR, for
hosts built without the lua-native feature (`render --precomputed DIR`).
";

#[derive(Default)]
struct Options {
    command: String,
    paths: Vec<PathBuf>,
    catalogues: Option<PathBuf>,
    inventory: Option<PathBuf>,
    verify: bool,
    json: bool,
    verbose: bool,
    out: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    precomputed: Option<PathBuf>,
    center: Option<[f64; 2]>,
    zoom: Option<f64>,
    scale: Option<f64>,
    size: Option<(u32, u32)>,
    density: Option<f64>,
    palette: Option<String>,
}

fn parse_pair<T: std::str::FromStr>(text: &str, separator: char) -> Option<(T, T)> {
    let (a, b) = text.split_once(separator)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next().with_context(|| format!("{flag} needs a value"))
}

fn parse(args: Vec<String>) -> Result<Options> {
    let mut args = args.into_iter();
    let mut options = Options {
        command: args.next().context("Missing command")?,
        ..Options::default()
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--catalogues" => options.catalogues = Some(value(&mut args, &arg)?.into()),
            "--inventory" => options.inventory = Some(value(&mut args, &arg)?.into()),
            "--verify" => options.verify = true,
            "--json" => options.json = true,
            "-v" | "--verbose" => options.verbose = true,
            "--out" | "-o" => options.out = Some(value(&mut args, &arg)?.into()),
            "--cache-dir" => options.cache_dir = Some(value(&mut args, &arg)?.into()),
            "--precomputed" => options.precomputed = Some(value(&mut args, &arg)?.into()),
            "--center" => {
                let text = value(&mut args, &arg)?;
                let (lat, lon) = parse_pair::<f64>(&text, ',')
                    .with_context(|| format!("--center LAT,LON, not {text}"))?;
                ensure!(
                    (-90.0..=90.0).contains(&lat) && lon.is_finite(),
                    "--center out of range"
                );
                options.center = Some([lat, lon]);
            }
            "--zoom" => options.zoom = Some(value(&mut args, &arg)?.parse().context("--zoom")?),
            "--scale" => {
                let text = value(&mut args, &arg)?;
                let text = text.strip_prefix("1:").unwrap_or(&text);
                options.scale = Some(text.parse().context("--scale")?);
            }
            "--size" => {
                let text = value(&mut args, &arg)?;
                let size = parse_pair::<u32>(&text.to_ascii_lowercase(), 'x')
                    .with_context(|| format!("--size WxH, not {text}"))?;
                ensure!(
                    size.0 > 0 && size.1 > 0 && size.0 <= 16384 && size.1 <= 16384,
                    "--size must be within 1..16384"
                );
                options.size = Some(size);
            }
            "--density" => {
                options.density = Some(value(&mut args, &arg)?.parse().context("--density")?)
            }
            "--palette" => options.palette = Some(value(&mut args, &arg)?),
            flag if flag.starts_with('-') => bail!("Unknown option {flag}"),
            _ => options.paths.push(arg.into()),
        }
    }
    ensure!(
        !(options.zoom.is_some() && options.scale.is_some()),
        "Use --zoom or --scale, not both"
    );
    Ok(options)
}

fn catalogue_folder(options: &Options) -> PathBuf {
    options
        .catalogues
        .clone()
        .or_else(|| std::env::var_os("FERRITE_CATALOGUES").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("Catalogues"))
}

fn open_engine(options: &Options, platform: Platform) -> Result<ChartEngine> {
    ensure!(!options.paths.is_empty(), "No dataset paths given");
    let catalogues = catalogue_folder(options);
    let mut config = engine_config(platform, &catalogues);
    config.inventory = options.inventory.clone();
    if options.verify {
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
    ChartEngine::new(config)
        .with_context(|| format!("Cannot load catalogues from {}", catalogues.display()))
}

fn inspect(options: &Options) -> Result<ExitCode> {
    let mut engine = open_engine(options, Platform::native())?;
    let report = open_all(&mut engine, sources_for_paths(&options.paths))?;
    if options.json {
        println!("{:#}", inspection_json(&engine, &report));
        return Ok(ExitCode::SUCCESS);
    }
    for note in engine.catalogue_errors() {
        println!("note: {note}");
    }
    for notice in &report.notices {
        println!("note: {notice}");
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
    let errors = report
        .issues
        .iter()
        .filter(|i| i.severity == IssueSeverity::Error)
        .count();
    println!(
        "\n{} validation issue(s): {} error(s), {} warning(s)",
        report.issues.len(),
        errors,
        report.issues.len() - errors
    );
    for issue in &report.issues {
        println!("  {issue}");
    }
    Ok(ExitCode::SUCCESS)
}

fn validate(options: &Options) -> Result<ExitCode> {
    let mut engine = open_engine(options, Platform::native())?;
    let mut report = open_all(&mut engine, sources_for_paths(&options.paths))?;
    let (exchange_issues, exchange_notices) = engine.exchange_set_checks();
    report.issues.extend(exchange_issues);
    report.notices.extend(exchange_notices);
    let errors = report
        .issues
        .iter()
        .filter(|i| i.severity == IssueSeverity::Error)
        .count();
    let warnings = report.issues.len() - errors;
    if options.json {
        println!(
            "{:#}",
            json!({
                "valid": errors == 0,
                "datasets": engine.datasets().len(),
                "errors": errors,
                "warnings": warnings,
                "issues": report.issues.iter().map(issue_json).collect::<Vec<_>>(),
                "notices": report.notices,
            })
        );
    } else {
        for notice in &report.notices {
            println!("note: {notice}");
        }
        for issue in &report.issues {
            println!("{issue}");
        }
        println!(
            "{} dataset(s): {} error(s), {} warning(s) - {}",
            engine.datasets().len(),
            errors,
            warnings,
            if errors == 0 { "valid" } else { "INVALID" }
        );
    }
    Ok(if errors == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// The view, viewport and display options of `render`/`precompute`.
fn view(options: &Options, engine: &mut ChartEngine) -> Result<(View, f64)> {
    let (width, height) = options.size.unwrap_or((1920, 1018));
    let viewport = ViewportPixels {
        width,
        height,
        scale_factor: options.density.unwrap_or(1.0),
    };
    if let Some(palette) = &options.palette {
        let available = palettes(engine);
        ensure!(
            available.iter().any(|p| p == palette),
            "Unknown palette {palette} (available: {})",
            available.join(", ")
        );
        engine.set_display(DisplayOptions {
            profile: palette.clone(),
            ..engine.display().clone()
        })?;
    }
    let camera = Camera {
        center: options.center,
        zoom: match options.scale {
            Some(denominator) => Zoom::Scale(denominator),
            None => Zoom::Factor(options.zoom.unwrap_or(1.0)),
        },
    };
    let (_, zoom) = engine.camera_view(camera, viewport)?;
    Ok((
        View {
            fit: None,
            viewport,
            camera: Some(camera),
        },
        zoom,
    ))
}

fn report_open(report: &ferrite_engine::chart_engine::OpenReport) -> Result<()> {
    for notice in &report.notices {
        eprintln!("note: {notice}");
    }
    for issue in &report.issues {
        eprintln!("{issue}");
    }
    ensure!(!report.datasets.is_empty(), "No datasets opened");
    Ok(())
}

fn render(options: &Options) -> Result<ExitCode> {
    let out = options.out.clone().context("render needs --out FILE.png")?;
    let platform = match &options.precomputed {
        Some(dir) => Platform::native().with_cache(Arc::new(
            DirectoryCacheStore::new(dir)
                .with_context(|| format!("Cannot use {}", dir.display()))?,
        )),
        None => Platform::native(),
    };
    let mut engine = open_engine(options, platform)?;
    if options.precomputed.is_some() {
        engine.set_portrayal_source(PortrayalSource::Precomputed);
    }
    report_open(&open_all(&mut engine, sources_for_paths(&options.paths))?)?;
    let (view, zoom) = view(options, &mut engine)?;
    let started = std::time::Instant::now();
    let scene = engine.prepare(&view)?;
    let prepared = started.elapsed();
    let mut scene =
        Arc::try_unwrap(scene).map_err(|_| anyhow::anyhow!("Prepared scene is shared"))?;
    let mut renderer = OffscreenChartRenderer::new(
        view.viewport.width,
        view.viewport.height,
        view.viewport.scale_factor,
    )?;
    let image = renderer.render(&engine, &mut scene, zoom)?;
    save_png(&image, &out)?;
    eprintln!(
        "{}: {}x{} ({} instructions, {}, zoom {zoom:.3}, portrayal {:.1} s, total {:.1} s, {})",
        out.display(),
        image.width(),
        image.height(),
        scene.instruction_count(),
        engine.display().profile,
        prepared.as_secs_f64(),
        started.elapsed().as_secs_f64(),
        renderer.adapter(),
    );
    Ok(ExitCode::SUCCESS)
}

fn precompute(options: &Options) -> Result<ExitCode> {
    let dir = options
        .cache_dir
        .clone()
        .context("precompute needs --cache-dir DIR")?;
    let store = Arc::new(
        DirectoryCacheStore::new(&dir).with_context(|| format!("Cannot use {}", dir.display()))?,
    );
    let mut engine = open_engine(options, Platform::native().with_cache(store))?;
    ensure!(
        engine.portrayal_source() == PortrayalSource::Lua,
        "precompute needs Lua portrayal (built without the lua-native feature)"
    );
    report_open(&open_all(&mut engine, sources_for_paths(&options.paths))?)?;
    let (view, _) = view(options, &mut engine)?;
    engine.set_precomputed_export(true);
    let scene = engine.prepare(&view)?;
    let written = std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".ferrite_pre_")
        })
        .count();
    eprintln!(
        "{}: {} precomputed cell payload(s) ({} instructions, {})",
        dir.display(),
        written,
        scene.instruction_count(),
        engine.display().profile
    );
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return if args.is_empty() {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
    }
    if args[0] == "--version" {
        println!("ferrite {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let result = parse(args).and_then(|options| {
        let filter = std::env::var("RUST_LOG")
            .unwrap_or_else(|_| if options.verbose { "info" } else { "error" }.to_string());
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .try_init();
        match options.command.as_str() {
            "inspect" => inspect(&options),
            "validate" => validate(&options),
            "render" => render(&options),
            "precompute" => precompute(&options),
            other => bail!("Unknown command {other}; see ferrite --help"),
        }
    });
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_view_options() {
        let options = parse(args(
            "render a b --out x.png --center 49.2,-2.1 --scale 1:50000 --size 800X600 --palette Night",
        ))
        .unwrap();
        assert_eq!(options.paths.len(), 2);
        assert_eq!(options.center, Some([49.2, -2.1]));
        assert_eq!(options.scale, Some(50000.));
        assert_eq!(options.size, Some((800, 600)));
        assert_eq!(options.palette.as_deref(), Some("Night"));
        assert!(parse(args("render --zoom 2 --scale 3")).is_err());
        assert!(parse(args("render --center 95,0")).is_err());
        assert!(parse(args("render --bogus")).is_err());
        assert_eq!(ferrite_cli::severity_name(IssueSeverity::Error), "error");
    }
}
