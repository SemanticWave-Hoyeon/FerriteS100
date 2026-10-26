//! Feature and portrayal catalogue loading, status and cache identity.
use crate::dataset_discovery;
use anyhow::{Context, Result};
use ferrite_feature_catalog::{BoundFeatureCatalogue, FeatureCatalogue};
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, PortrayalCatalogue};
use ferrite_render::host_settings::CatalogueStatus;
use std::path::Path;
use tracing::{debug, info};

/// Validate Feature Catalogue and create status
pub fn validate_fc(fc: &FeatureCatalogue, path: &Path) -> CatalogueStatus {
    let mut validation_messages = Vec::new();

    // Check product ID
    if fc.product_id.is_empty() {
        validation_messages.push("Missing product ID");
    } else if !fc.product_id.contains("S-101") && fc.product_id != "S-101" {
        validation_messages.push("Product ID is not S-101");
    }

    // Check required content
    if fc.feature_types.is_empty() {
        validation_messages.push("No feature types defined");
    }
    if fc.simple_attributes.is_empty() {
        validation_messages.push("No simple attributes defined");
    }

    // Check for essential S-101 feature types
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

/// Validate Portrayal Catalogue and create status
pub fn validate_pc(pc: &PortrayalCatalogue, path: &Path) -> CatalogueStatus {
    let mut validation_messages = Vec::new();

    // Check product ID
    if pc.product_id.is_empty() {
        validation_messages.push("Missing product ID");
    } else if !pc.product_id.contains("S-101") && pc.product_id != "S-101" {
        validation_messages.push("Product ID is not S-101");
    }

    // Check required content
    if pc.color_profiles.profiles.is_empty() {
        validation_messages.push("No color profiles defined");
    }
    if pc.symbols.symbols.is_empty() {
        validation_messages.push("No symbols defined");
    }

    // Check for required color profiles (Day, Dusk, Night)
    let required_profiles = ["Day", "Dusk", "Night"];
    let missing: Vec<_> = required_profiles
        .iter()
        .filter(|p| !pc.color_profiles.profiles.contains_key(**p))
        .collect();
    if !missing.is_empty() {
        validation_messages.push("Missing required color profiles");
    }

    if let Ok((_, diagnostics)) = ferrite_s101::context_validation_parameters(pc) {
        if !diagnostics.is_empty() {
            validation_messages
                .push("Known language-rule defect corrected locally; original catalogue retained");
        }
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

/// Load Feature Catalogue from XML file
pub fn load_feature_catalogue(path: &Path) -> Result<BoundFeatureCatalogue> {
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

    // If path is a directory, scan for FC XML file
    let fc_file = if path.is_dir() {
        dataset_discovery::feature_catalogue_file(path)?
    } else {
        path.to_path_buf()
    };

    let fc = FeatureCatalogue::load_bound(&fc_file)
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

    let pc = PortrayalCatalogue::load_bound(path)
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

/// The persistent cache uses retained parser/runtime inputs, never the live tree.
pub fn hash_bound_catalogue_inputs<H: std::hash::Hasher>(
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    hash: &mut H,
) {
    use std::hash::Hash;
    fc.version.hash(hash);
    fc.source_digest().hash(hash);
    pc.source_digest().hash(hash);
}

#[cfg(test)]
mod bound_catalogue_cache_tests {
    use super::*;
    #[test]
    fn actual_catalogue_key_component_follows_bound_objects_not_live_edits() {
        use std::hash::Hasher;
        let root = std::env::temp_dir().join(format!(
            "ferrite-app-catalogue-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("PC/Rules")).unwrap();
        let fc_path = root.join("FC.xml");
        let pc_path = root.join("PC");
        let xml = |name: &str| {
            format!(
                "<S100_FC_FeatureCatalogue><name>{name}</name><versionNumber>2.0</versionNumber><productId>S-101</productId></S100_FC_FeatureCatalogue>"
            )
        };
        std::fs::write(&fc_path, xml("A")).unwrap();
        std::fs::write(pc_path.join("portrayal_catalogue.xml"), "<portrayalCatalog productId='S-101' version='2.0'><foundationMode/><displayPlanes><displayPlane id='OverRadar' order='1'/></displayPlanes></portrayalCatalog>").unwrap();
        std::fs::write(pc_path.join("Rules/main.lua"), "return 'A'").unwrap();
        let fc_a = FeatureCatalogue::load_bound(&fc_path).unwrap();
        let pc_a = PortrayalCatalogue::load_bound(&pc_path).unwrap();
        let key = |fc: &BoundFeatureCatalogue, pc: &BoundPortrayalCatalogue| {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            hash_bound_catalogue_inputs(fc, pc, &mut hash);
            hash.finish()
        };
        let a = key(&fc_a, &pc_a);
        std::fs::write(&fc_path, xml("B")).unwrap();
        std::fs::write(pc_path.join("Rules/main.lua"), "return 'B'").unwrap();
        assert_eq!(key(&fc_a, &pc_a), a);
        let fc_b = FeatureCatalogue::load_bound(&fc_path).unwrap();
        let pc_b = PortrayalCatalogue::load_bound(&pc_path).unwrap();
        assert_ne!(key(&fc_b, &pc_a), a);
        assert_ne!(key(&fc_a, &pc_b), a);
        let b = key(&fc_b, &pc_b);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(key(&fc_a, &pc_a), a);
        assert_eq!(key(&fc_b, &pc_b), b);
    }
}
