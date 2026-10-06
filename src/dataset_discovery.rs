//! Application directory discovery; filesystem metadata is not product data.
use std::path::Path;

pub fn is_dataset_file(path: &Path, extension: &str) -> bool {
    !path
        .file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"._"))
        && path
            .extension()
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

/// FC folders must resolve deterministically to one real XML file.
pub fn feature_catalogue_file(root: &Path) -> anyhow::Result<std::path::PathBuf> {
    let mut files = std::fs::read_dir(root)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.retain(|p| p.is_file() && is_dataset_file(p, "xml"));
    files.sort();
    anyhow::ensure!(files.len() == 1, "FC folder must contain exactly one catalogue XML (found {}); select an explicit file for multiple versions", files.len());
    Ok(files.remove(0))
}

/// Discover S-101 chain members and S-102 products without following symlinks.
/// The application planner groups numeric members; updates are never standalone cells.
pub fn discover_exchange_folder(
    root: &Path,
) -> anyhow::Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>)> {
    anyhow::ensure!(root.is_dir(), "Exchange set path is not a folder");
    let mut charts = Vec::new();
    let mut rasters = Vec::new();
    let mut entries = 0usize;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        entries += 1;
        anyhow::ensure!(
            entries <= 100_000,
            "Exchange set discovery exceeds entry limit"
        );
        if entry.file_type().is_file() {
            if crate::s101_update_plan::is_chart_file(entry.path()) {
                charts.push(entry.into_path());
            } else if is_dataset_file(entry.path(), "h5") {
                rasters.push(entry.into_path());
            }
        }
    }
    charts.sort();
    rasters.sort();
    anyhow::ensure!(
        !charts.is_empty() || !rasters.is_empty(),
        "No S-101 chain members or S-102 HDF5 files found"
    );
    Ok((charts, rasters))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scans_product_files_without_appledouble_metadata() {
        for (name, extension, expected) in [
            ("101FR00369660.000", "000", true),
            ("._101FR00369660.000", "000", false),
            ("102FR00SMALOG000001.H5", "h5", true),
            ("102FR00SMALOG000001.h5", "h5", true),
            ("._102FR00SMALOG000001.H5", "h5", false),
            ("102FR00SMALOG000001.H5", "000", false),
        ] {
            assert_eq!(
                is_dataset_file(Path::new(name), extension),
                expected,
                "{name}"
            );
        }
    }
    #[test]
    fn discovers_nested_chain_members_without_metadata() {
        let root = std::env::temp_dir().join(format!("ferrite-discovery-{}", std::process::id()));
        std::fs::create_dir_all(root.join("S100_ROOT/datasets")).unwrap();
        let folder = root.join("S100_ROOT/datasets");
        for name in ["ENC.000", "ENC.001", "._ENC.000", "BATHY.H5", "._BATHY.H5"] {
            std::fs::write(folder.join(name), []).unwrap();
        }
        let (charts, rasters) = discover_exchange_folder(&root).unwrap();
        assert_eq!(charts, vec![folder.join("ENC.000"), folder.join("ENC.001")]);
        assert_eq!(rasters, vec![folder.join("BATHY.H5")]);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod catalogue_discovery_tests {
    use super::*;
    #[test]
    fn appledouble_is_excluded_and_ambiguous_versions_are_rejected() {
        let root =
            std::env::temp_dir().join(format!("ferrite-fc-discovery-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("._101_Feature_Catalogue_2.0.0.xml"),
            [0, 5, 22, 7],
        )
        .unwrap();
        assert!(feature_catalogue_file(&root).is_err());
        let real = root.join("101_Feature_Catalogue_2.0.0.xml");
        std::fs::write(&real, "<catalogue/>").unwrap();
        assert_eq!(feature_catalogue_file(&root).unwrap(), real);
        std::fs::write(root.join("101_Feature_Catalogue_2.1.0.xml"), "<catalogue/>").unwrap();
        assert!(feature_catalogue_file(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
