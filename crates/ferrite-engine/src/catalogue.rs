//! Feature and portrayal catalogue loading (the S-101 status checks and cache
//! identity are `ferrite_s101::layer::catalogue`).
use crate::dataset_discovery;
use crate::platform::{DataSource, NativeDataSource};
use anyhow::{Context, Result};
use ferrite_feature_catalog::{BoundFeatureCatalogue, FeatureCatalogue};
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, PortrayalCatalogue};
use std::path::Path;
use tracing::{debug, info};

/// Load Feature Catalogue from XML file
pub fn load_feature_catalogue(path: &Path) -> Result<BoundFeatureCatalogue> {
    load_feature_catalogue_from(&NativeDataSource, path)
}

/// [`load_feature_catalogue`] through a data source.
pub fn load_feature_catalogue_from(
    data: &dyn DataSource,
    path: &Path,
) -> Result<BoundFeatureCatalogue> {
    info!("Loading Feature Catalogue: {}", path.display());

    if !data.exists(path) {
        anyhow::bail!(
            "Feature Catalogue directory not found: {}\n\
             Expected layout: <app>/Catalogues/FC/S-101/*Feature_Catalogue*.xml.\n\
             If you launched the executable from a build/output directory, ensure the \
             Catalogues/ folder is present alongside it.",
            path.display()
        );
    }

    // If path is a directory, scan for FC XML file
    let fc_file = if data.is_dir(path) {
        dataset_discovery::feature_catalogue_file_in(data, path)?
    } else {
        path.to_path_buf()
    };

    let fc = data
        .read_limited(&fc_file, FeatureCatalogue::MAX_XML_SIZE)
        .map_err(ferrite_feature_catalog::FCError::from)
        .and_then(|bytes| FeatureCatalogue::bound_from_bytes(&fc_file, &bytes))
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

/// Load Portrayal Catalogue from directory.
///
/// Failure here is fatal: an empty PC means no color profiles and no symbols,
/// which would silently degrade every chart render to placeholder fallbacks.
/// Surfacing the error early forces the user to fix the install/path instead
/// of seeing a broken render.
pub fn load_portrayal_catalogue(path: &Path) -> Result<BoundPortrayalCatalogue> {
    load_portrayal_catalogue_from(&NativeDataSource, path)
}

/// [`load_portrayal_catalogue`] through a data source.
pub fn load_portrayal_catalogue_from(
    data: &dyn DataSource,
    path: &Path,
) -> Result<BoundPortrayalCatalogue> {
    info!("Loading Portrayal Catalogue: {}", path.display());

    if !data.exists(path) {
        anyhow::bail!(
            "Portrayal Catalogue directory not found: {}\n\
             Expected layout: <app>/Catalogues/PC/S-101/ with ColorProfiles/, Symbols/, Rules/.\n\
             If you launched the executable from a build/output directory, ensure the \
             Catalogues/ folder is present alongside it.",
            path.display()
        );
    }

    let pc = data
        .portrayal_sources(path)
        .and_then(|sources| PortrayalCatalogue::bound_from_sources(path, sources))
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
        // Log some sample colors
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
