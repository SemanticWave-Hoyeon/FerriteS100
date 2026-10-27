//! S-101 feature and portrayal catalogue status (for the catalogue panel)
//! and the catalogue part of the cell cache identity.
use ferrite_feature_catalog::{BoundFeatureCatalogue, FeatureCatalogue};
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, PortrayalCatalogue};
use ferrite_render::host_settings::CatalogueStatus;
use std::path::Path;

/// Where the S-101 catalogues are installed (catalogue load errors).
pub const LAYOUT: ferrite_product_api::CatalogueLayout = ferrite_product_api::CatalogueLayout {
    feature: "<app>/Catalogues/FC/S-101/*Feature_Catalogue*.xml",
    portrayal: "<app>/Catalogues/PC/S-101/ with ColorProfiles/, Symbols/, Rules/",
};

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

    if let Ok((_, diagnostics)) = crate::context_validation_parameters(pc) {
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
