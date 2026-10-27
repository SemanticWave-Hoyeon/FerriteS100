//! Application directory discovery; filesystem metadata is not product data.
use crate::platform::{DataSource, NativeDataSource};
use std::path::Path;

pub use ferrite_product_api::platform::is_dataset_file;

/// Recursive discovery of S-101 chain members and S-102 files (the open-all
/// planner), defined with the S-101 update planner.
pub use ferrite_s101::layer::discovery::{discover_recursive_files, discover_recursive_files_in};

/// FC folders must resolve deterministically to one real XML file.
pub fn feature_catalogue_file(root: &Path) -> anyhow::Result<std::path::PathBuf> {
    feature_catalogue_file_in(&NativeDataSource, root)
}

/// [`feature_catalogue_file`] through a data source.
pub fn feature_catalogue_file_in(
    data: &dyn DataSource,
    root: &Path,
) -> anyhow::Result<std::path::PathBuf> {
    let mut files: Vec<_> = data.read_dir(root)?.into_iter().map(|e| e.path).collect();
    files.retain(|p| data.is_file(p) && is_dataset_file(p, "xml"));
    files.sort();
    anyhow::ensure!(files.len() == 1, "FC folder must contain exactly one catalogue XML (found {}); select an explicit file for multiple versions", files.len());
    Ok(files.remove(0))
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
