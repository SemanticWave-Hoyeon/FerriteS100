use anyhow::{Context, Result};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_lua::{PortrayalEngine, TypeCatalogue};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_wgpu::SymbolCache;
use std::path::PathBuf;
fn main() -> Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).context("Pass catalogue root")?);
    let mut rows = Vec::new();
    for version in ["1.0.2", "1.1.0", "2.0.0", "2.1.0"] {
        let base = root.join(version);
        // Select the catalogue explicitly; directory order may put AppleDouble
        // sidecars before XML on Windows.
        let fcpath = base
            .join("FC")
            .join(format!("101_Feature_Catalogue_{version}.xml"));
        let fc = FeatureCatalogue::load(fcpath)?;
        let mut use_counts = std::collections::BTreeMap::<String, usize>::new();
        for f in fc.feature_types.values() {
            let kind = f
                .feature_use_type
                .with_context(|| format!("Missing featureUseType in {version}: {}", f.code))?;
            *use_counts.entry(kind.as_str().into()).or_default() += 1;
        }
        anyhow::ensure!(
            fc.feature_types
                .get("Wreck")
                .and_then(|f| f.feature_use_type)
                == Some(ferrite_feature_catalog::FeatureUseType::Geographic),
            "Wreck misclassified in {version}"
        );
        anyhow::ensure!(
            fc.feature_types
                .get("QualityOfBathymetricData")
                .and_then(|f| f.feature_use_type)
                == Some(ferrite_feature_catalog::FeatureUseType::Meta),
            "Quality meta-feature misclassified in {version}"
        );
        let pc = PortrayalCatalogue::load(base.join("PC"))?;
        let mut engine = PortrayalEngine::new(pc.root_path.join("Rules"))?;
        engine.set_type_catalogue(TypeCatalogue::from_feature_catalogue(&fc));
        engine.initialize()?;
        let mut failed = Vec::new();
        let mut rendered = 0;
        for profile in pc.color_profiles.profiles.values() {
            let mut cache = SymbolCache::new(base.join("PC/Symbols"));
            for entry in std::fs::read_dir(base.join("PC/Symbols"))? {
                let path = entry?.path();
                if path.is_file()
                    && !path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("._")
                    && path.extension().is_some_and(|s| s == "svg")
                {
                    let id = path.file_stem().unwrap().to_string_lossy();
                    if cache.get_symbol(&id, profile).is_some() {
                        rendered += 1;
                    } else {
                        failed.push(format!("{}:{}", profile.name, id));
                    }
                }
            }
        }
        anyhow::ensure!(
            failed.is_empty(),
            "Symbol profile rendering failed in {version}: {failed:?}"
        );
        rows.push(serde_json::json!({"version":version,"fc_version":fc.version,"pc_version":pc.version,"feature_use_counts":use_counts,"feature_types":fc.feature_types.len(),"lua_initialized":true,"symbol_profile_renders":rendered,"failed_symbols":failed}));
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}
