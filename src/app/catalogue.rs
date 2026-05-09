//! Feature / Portrayal Catalogue loading + validation, plus a simple
//! `ChartData/` scanner used by tests.
//!
//! Failure semantics: missing or empty catalogues are fatal because the
//! renderer relies on a real color profile and symbol set; silently
//! substituting empty defaults caused the red-square fallback bug we
//! fixed in commit 70bd150.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_s100_core::S101Cell;
use ferrite_wgpu::CatalogueStatus;

/// Validate the loaded Feature Catalogue and produce a status struct for the UI.
pub fn validate_fc(fc: &FeatureCatalogue, path: &Path) -> CatalogueStatus {
    let mut validation_messages = Vec::new();

    if fc.product_id.is_empty() {
        validation_messages.push("Missing product ID");
    } else if !fc.product_id.contains("S-101") && fc.product_id != "S-101" {
        validation_messages.push("Product ID is not S-101");
    }

    if fc.feature_types.is_empty() {
        validation_messages.push("No feature types defined");
    }
    if fc.simple_attributes.is_empty() {
        validation_messages.push("No simple attributes defined");
    }

    let essential_features = ["DepthArea", "LandArea", "Coastline", "Sounding"];
    let missing: Vec<_> = essential_features
        .iter()
        .filter(|f| !fc.feature_types.contains_key(**f))
        .collect();
    if !missing.is_empty() {
        validation_messages.push("Missing essential feature types");
    }

    let validation_message = if validation_messages.is_empty() {
        Some("Valid S-101 Feature Catalogue".to_string())
    } else {
        Some(format!("Warning: {}", validation_messages.join(", ")))
    };

    CatalogueStatus {
        loaded: true,
        product_id: fc.product_id.clone(),
        version: fc.version.clone(),
        path: path.display().to_string(),
        item_count: fc.feature_types.len(),
        validation_message,
    }
}

/// Validate the loaded Portrayal Catalogue and produce a status struct for the UI.
pub fn validate_pc(pc: &PortrayalCatalogue, path: &Path) -> CatalogueStatus {
    let mut validation_messages = Vec::new();

    if pc.product_id.is_empty() {
        validation_messages.push("Missing product ID");
    } else if !pc.product_id.contains("S-101") && pc.product_id != "S-101" {
        validation_messages.push("Product ID is not S-101");
    }

    if pc.color_profiles.profiles.is_empty() {
        validation_messages.push("No color profiles defined");
    }
    if pc.symbols.symbols.is_empty() {
        validation_messages.push("No symbols defined");
    }

    let required_profiles = ["Day", "Dusk", "Night"];
    let missing: Vec<_> = required_profiles
        .iter()
        .filter(|p| !pc.color_profiles.profiles.contains_key(**p))
        .collect();
    if !missing.is_empty() {
        validation_messages.push("Missing required color profiles");
    }

    let validation_message = if validation_messages.is_empty() {
        Some("Valid S-101 Portrayal Catalogue".to_string())
    } else {
        Some(format!("Warning: {}", validation_messages.join(", ")))
    };

    CatalogueStatus {
        loaded: true,
        product_id: pc.product_id.clone(),
        version: pc.version.clone(),
        path: path.display().to_string(),
        item_count: pc.symbols.symbols.len(),
        validation_message,
    }
}

/// Load Feature Catalogue from a path (file or directory containing the FC XML).
pub fn load_feature_catalogue(path: &Path) -> Result<FeatureCatalogue> {
    info!("Loading Feature Catalogue: {}", path.display());

    if !path.exists() {
        anyhow::bail!(
            "Feature Catalogue directory not found: {}\n\
             Expected layout: <app>/Catalogues/FC/S-101/*Feature_Catalogue*.xml.\n\
             If you launched the executable from a build/output directory, ensure the \
             Catalogues/ folder is present alongside it.",
            path.display()
        );
    }

    // If `path` is a directory, scan it for the FC XML.
    let fc_file = if path.is_dir() {
        let mut found_fc = None;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let file_path = entry.path();
            if file_path.is_file() {
                if let Some(name) = file_path.file_name().and_then(|n| n.to_str()) {
                    // e.g. "101_Feature_Catalogue_*.xml" or "*_FC.xml"
                    if name.contains("Feature_Catalogue") && name.ends_with(".xml") {
                        info!("Found FC XML: {}", file_path.display());
                        found_fc = Some(file_path);
                        break;
                    }
                }
            }
        }
        found_fc.unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };

    let fc = FeatureCatalogue::load(&fc_file)
        .with_context(|| format!("Failed to load Feature Catalogue: {}", fc_file.display()))?;

    info!("FC loaded successfully");
    debug!("  Product: {}", fc.product_id);
    debug!("  Version: {}", fc.version);
    debug!("  Feature types: {}", fc.feature_types.len());
    debug!("  Simple attributes: {}", fc.simple_attributes.len());
    debug!("  Complex attributes: {}", fc.complex_attributes.len());
    debug!("  Information types: {}", fc.information_types.len());

    Ok(fc)
}

/// Load Portrayal Catalogue from a directory.
///
/// Failure here is fatal: an empty PC means no color profiles and no symbols,
/// which would silently degrade every chart render to placeholder fallbacks.
/// Surfacing the error early forces the user to fix the install/path instead
/// of seeing a broken render.
pub fn load_portrayal_catalogue(path: &Path) -> Result<PortrayalCatalogue> {
    info!("Loading Portrayal Catalogue: {}", path.display());

    if !path.exists() {
        anyhow::bail!(
            "Portrayal Catalogue directory not found: {}\n\
             Expected layout: <app>/Catalogues/PC/S-101/ with ColorProfiles/, Symbols/, Rules/.\n\
             If you launched the executable from a build/output directory, ensure the \
             Catalogues/ folder is present alongside it.",
            path.display()
        );
    }

    let pc = PortrayalCatalogue::load(path)
        .with_context(|| format!("Failed to load Portrayal Catalogue: {}", path.display()))?;

    if pc.color_profiles.profiles.is_empty() {
        anyhow::bail!(
            "Portrayal Catalogue at {} contains no color profiles. \
             Check that ColorProfiles/*.xml exists and is well-formed.",
            path.display()
        );
    }
    if pc.symbols.symbols.is_empty() {
        anyhow::bail!(
            "Portrayal Catalogue at {} contains no symbols. \
             Check that Symbols/*.svg exists.",
            path.display()
        );
    }

    info!("PC loaded successfully");
    debug!("  Product: {}", pc.product_id);
    debug!("  Version: {}", pc.version);
    debug!("  Color profiles: {}", pc.color_profiles.profiles.len());
    for (profile_id, profile) in &pc.color_profiles.profiles {
        info!(
            "  Color profile '{}' (name: '{}'): {} colors",
            profile_id,
            profile.name,
            profile.colors.len()
        );
        // Log a few sample colors at debug level for diagnostics.
        for token in ["DEPVS", "DEPMS", "DEPMD", "DEPDW", "DEPIT", "LANDA"] {
            if let Some(color) = profile.get_srgb(token) {
                debug!("    {}: RGB({}, {}, {})", token, color.r, color.g, color.b);
            }
        }
    }
    debug!("  Symbols: {}", pc.symbols.symbols.len());
    debug!("  Line styles: {}", pc.line_styles.len());
    debug!("  Area fills: {}", pc.area_fills.len());

    Ok(pc)
}

/// Walk `path` for `.000` chart files and load them. Currently filters to a
/// single test fixture by name; the production load path goes through the
/// interactive UI and `--chart` CLI arg, not this helper.
#[allow(dead_code)]
pub fn load_chart_data(path: &Path) -> Result<Vec<S101Cell>> {
    info!("Scanning ChartData folder: {}", path.display());

    if !path.exists() {
        warn!("ChartData directory not found: {}", path.display());
        warn!("Creating directory...");
        fs::create_dir_all(path)
            .with_context(|| format!("Failed to create ChartData directory: {}", path.display()))?;
        return Ok(Vec::new());
    }

    let chart_files: Vec<PathBuf> = WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().is_some_and(|ext| ext == "000")
                && e.path()
                    .file_name()
                    .is_some_and(|name| name == "101GB00GB302045.000")
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    info!("Found {} chart files", chart_files.len());

    let mut cells = Vec::new();
    for chart_path in &chart_files {
        info!("Loading: {}", chart_path.display());
        match S101Cell::load(chart_path) {
            Ok(cell) => {
                let stats = cell.statistics();
                debug!("  -> {}", stats);
                cells.push(cell);
            }
            Err(e) => {
                error!("Failed to load {}: {}", chart_path.display(), e);
            }
        }
    }

    info!("Loaded {} cells successfully", cells.len());
    Ok(cells)
}
