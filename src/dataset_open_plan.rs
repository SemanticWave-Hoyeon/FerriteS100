//! Read-only opening queue. Discovery never authenticates or materializes cells.
//! All numeric S-101 members go through the existing authorized single batch:
//! DSID latest edition/reissue, complete chains, identical-operation merge and
//! conflicting-operation rejection remain owned by s101_update_plan.
use anyhow::{ensure, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// One loader batch, not independent update loads or unauthenticated DSID selection.
    pub charts: Vec<PathBuf>,
    /// Explicit physical files selected by the user. Folder discovery is never
    /// an explicit cancellation request; pass this separately to authorized_batch.
    pub selected_charts: Vec<PathBuf>,
    /// Caller queues each independently so one unsupported/malformed raster cannot
    /// prevent a different supported product from being opened.
    pub rasters: Vec<PathBuf>,
    pub notices: Vec<String>,
    pub routes: Vec<crate::s421_dataset_input::CapturedRouteInput>,
}

/// Exact supported S-102 reader edition. The format suffix is not a product claim.
fn supported_product(spec: &str) -> bool {
    spec == "INT.IHO.S-102.3.0.0"
}

const MAX_HDF_DISCOVERY_BYTES: u64 = 512 * 1024 * 1024;

fn checked_product_text(raw: &[u8]) -> Result<String> {
    ensure!(
        raw.len() <= 255 && !raw.contains(&0),
        "HDF productSpecification exceeds255 bytes or contains NUL"
    );
    Ok(std::str::from_utf8(raw)?.to_owned())
}

fn hdf_product(path: &Path) -> Result<String> {
    use ferrite_s102::hdf5::{
        types::{FixedAscii, FixedUnicode, TypeDescriptor, VarLenAscii, VarLenUnicode},
        File,
    };
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file(),
        "HDF discovery input must be a real file"
    );
    ensure!(
        metadata.len() <= MAX_HDF_DISCOVERY_BYTES,
        "HDF discovery file exceeds512MiB receiver limit"
    );
    let file = File::open(path)
        .with_context(|| format!("Cannot open HDF metadata: {}", path.display()))?;
    let attr = file
        .attr("productSpecification")
        .context("HDF has no productSpecification attribute")?;
    // Scalar per S-100 Part 10c; UKHO 2026 exchange sets use a one-element array.
    ensure!(
        ferrite_s102::singleton::admitted(&attr),
        "HDF productSpecification must be scalar or a single element"
    );
    use ferrite_s102::singleton::read as one;
    // Same-charset readers match the existing S102 adapter. The255-byte check
    // bounds the subsequent owned Rust text, not HDF's prior VarLen allocation.
    // File-size preflight is not an atomic capture or a decompressed/RSS limit.
    match attr.dtype()?.to_descriptor()? {
        TypeDescriptor::FixedAscii(n) => {
            ensure!(
                n <= 256,
                "HDF productSpecification exceeds256 encoded bytes"
            );
            checked_product_text(one::<FixedAscii<256>>(&attr)?.as_bytes())
        }
        TypeDescriptor::FixedUnicode(n) => {
            ensure!(
                n <= 256,
                "HDF productSpecification exceeds256 encoded bytes"
            );
            checked_product_text(one::<FixedUnicode<256>>(&attr)?.as_bytes())
        }
        TypeDescriptor::VarLenAscii => checked_product_text(one::<VarLenAscii>(&attr)?.as_bytes()),
        TypeDescriptor::VarLenUnicode => {
            checked_product_text(one::<VarLenUnicode>(&attr)?.as_bytes())
        }
        _ => anyhow::bail!("HDF productSpecification must be text"),
    }
}

fn unsupported_product_notice(spec: &str, path: &Path) -> String {
    if matches!(spec, "INT.IHO.S-102.2.1" | "INT.IHO.S-102.2.1.0") {
        return format!("S-102 2.1 not loaded: a compatible 2.1 reader is not available (installed reader: 3.0.0). Original data retained; no conversion performed. File: {}", path.display());
    }
    format!("Skipped unsupported product {spec}: {}", path.display())
}

fn classify_rasters(plan: &mut Plan, files: Vec<PathBuf>) {
    for path in files {
        match hdf_product(&path) {
            Ok(spec) if supported_product(&spec) => plan.rasters.push(path),
            Ok(spec) => plan.notices.push(unsupported_product_notice(&spec, &path)),
            Err(error) => plan.notices.push(format!(
                "Skipped HDF product with invalid metadata {}: {error:#}",
                path.display()
            )),
        }
    }
}

pub(crate) fn discover(root: &Path) -> Result<Plan> {
    let (charts, rasters) = crate::dataset_discovery::discover_recursive_files(root)?;
    let mut plan = Plan {
        charts,
        ..Plan::default()
    };
    classify_rasters(&mut plan, rasters);
    let (routes, notices) = crate::s421_dataset_input::discover_routes(root)?;
    plan.routes = routes;
    plan.notices.extend(notices);
    if plan.charts.is_empty()
        && plan.rasters.is_empty()
        && plan.routes.is_empty()
        && plan.notices.is_empty()
    {
        let mut archives = Vec::new();
        for (count, entry) in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .enumerate()
        {
            ensure!(count < 100_000, "Archive discovery exceeds entry limit");
            let entry = entry?;
            if entry.file_type().is_file()
                && crate::dataset_discovery::is_dataset_file(entry.path(), "zip")
            {
                archives.push(entry.into_path());
                if archives.len() == 3 {
                    break;
                }
            }
        }
        if archives.is_empty() {
            plan.notices.push(format!(
                "No supported dataset files found: {}",
                root.display()
            ));
        } else {
            plan.notices.push(format!(
                "This folder contains ZIP archives, not extracted dataset files. Extract the exchange set and open its S100_ROOT folder (or the prepared ExchangeSets folder). Archive: {}",
                archives[0].display()
            ));
        }
    }
    Ok(plan)
}

pub(crate) fn single_file(path: &Path) -> Result<Plan> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "Selected dataset must be a real file"
    );
    if path
        .file_name()
        .is_some_and(|v| v.eq_ignore_ascii_case("CATALOG.XML"))
    {
        return discover(path.parent().context("Catalogue has no parent directory")?);
    }
    if crate::s101_update_plan::is_chart_file(path) {
        // Existing load_charts expands siblings and verifies the original inputs.
        return Ok(Plan {
            charts: vec![path.to_path_buf()],
            selected_charts: vec![path.to_path_buf()],
            ..Plan::default()
        });
    }
    let mut plan = Plan::default();
    if crate::dataset_discovery::is_dataset_file(path, "h5")
        || crate::dataset_discovery::is_dataset_file(path, "hdf5")
    {
        classify_rasters(&mut plan, vec![path.to_path_buf()]);
    } else if crate::dataset_discovery::is_dataset_file(path, "gml")
        || crate::dataset_discovery::is_dataset_file(path, "xml")
    {
        match crate::s421_dataset_input::capture_if_route(path)? {
            Some(input) => plan.routes.push(input),
            None => plan
                .notices
                .push(format!("Skipped non-S421 XML: {}", path.display())),
        }
    } else {
        plan.notices.push(format!(
            "Skipped unsupported dataset file: {}",
            path.display()
        ));
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s102::hdf5::{
        types::{FixedAscii, VarLenAscii, VarLenUnicode},
        File,
    };
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("ferrite-open-plan-{}-{n}", std::process::id()));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn nested_deliveries_preserve_all_original_chain_paths_not_filename_edition_guess() {
        let tmp = TestDir::new();
        let a = tmp.path().join("edition-a/S100_ROOT");
        let b = tmp.path().join("edition-b/S100_ROOT");
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("CATALOG.XML"), []).unwrap();
            for name in ["CELL.000", "CELL.001"] {
                std::fs::write(d.join(name), []).unwrap();
            }
        }
        std::fs::write(a.join("._CELL.000"), []).unwrap();
        let p = discover(tmp.path()).unwrap();
        assert_eq!(
            p.charts,
            vec![
                a.join("CELL.000"),
                a.join("CELL.001"),
                b.join("CELL.000"),
                b.join("CELL.001")
            ]
        );
        assert!(p.rasters.is_empty());
        assert!(p.selected_charts.is_empty());
        let selected = single_file(&a.join("CELL.001")).unwrap();
        assert_eq!(selected.selected_charts, vec![a.join("CELL.001")]);
        assert_eq!(
            single_file(&a.join("CATALOG.XML")).unwrap().charts,
            vec![a.join("CELL.000"), a.join("CELL.001")]
        );
    }
    #[test]
    fn ukho_21_has_explicit_reader_diagnostic_without_being_relabelled_30() {
        let tmp = TestDir::new();
        for (i, spec) in ["INT.IHO.S-102.2.1", "INT.IHO.S-102.2.1.0"]
            .into_iter()
            .enumerate()
        {
            let path = tmp.path().join(format!("ukho{i}.h5"));
            {
                let f = File::create(&path).unwrap();
                f.new_attr::<VarLenAscii>()
                    .create("productSpecification")
                    .unwrap()
                    .write_scalar(&VarLenAscii::from_ascii(spec).unwrap())
                    .unwrap();
            }
            let plan = single_file(&path).unwrap();
            assert!(plan.rasters.is_empty());
            assert_eq!(plan.notices.len(), 1);
            assert!(plan.notices[0].contains("compatible 2.1 reader"));
            assert!(plan.notices[0].contains("no conversion performed"));
            assert_eq!(hdf_product(&path).unwrap(), spec);
        }
    }
    #[test]
    fn product_attribute_not_extension_controls_hdf_routing() {
        let tmp = TestDir::new();
        for (name, spec) in [
            ("s102.H5", "INT.IHO.S-102.3.0.0"),
            ("s111.h5", "INT.IHO.S-111.2.0.0"),
            ("old.h5", "INT.IHO.S-102.2.2.0"),
        ] {
            let f = File::create(tmp.path().join(name)).unwrap();
            f.new_attr::<FixedAscii<64>>()
                .create("productSpecification")
                .unwrap()
                .write_scalar(&FixedAscii::<64>::from_ascii(spec).unwrap())
                .unwrap();
        }
        std::fs::write(tmp.path().join("malformed.h5"), b"not hdf").unwrap();
        let p = discover(tmp.path()).unwrap();
        assert_eq!(p.rasters, vec![tmp.path().join("s102.H5")]);
        assert_eq!(p.notices.len(), 3);
    }
    #[test]
    fn variable_attribute_same_charset_reads_supported_ascii_unicode() {
        let tmp = TestDir::new();
        let spec = "INT.IHO.S-102.3.0.0";
        let a = tmp.path().join("a.h5");
        {
            let f = File::create(&a).unwrap();
            f.new_attr::<VarLenAscii>()
                .create("productSpecification")
                .unwrap()
                .write_scalar(&VarLenAscii::from_ascii(spec).unwrap())
                .unwrap();
        }
        let b = tmp.path().join("b.h5");
        {
            let f = File::create(&b).unwrap();
            f.new_attr::<VarLenUnicode>()
                .create("productSpecification")
                .unwrap()
                .write_scalar(&spec.parse::<VarLenUnicode>().unwrap())
                .unwrap();
        }
        assert_eq!(hdf_product(&a).unwrap(), spec);
        assert_eq!(hdf_product(&b).unwrap(), spec);
    }
    #[test]
    fn hdf_file_size_preflight_and_postread_text_limits_are_distinct() {
        let tmp = TestDir::new();
        let oversized = tmp.path().join("oversized.h5");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_HDF_DISCOVERY_BYTES + 1)
            .unwrap();
        assert!(hdf_product(&oversized)
            .unwrap_err()
            .to_string()
            .contains("512MiB"));
        let text = tmp.path().join("long.h5");
        {
            let f = File::create(&text).unwrap();
            f.new_attr::<VarLenAscii>()
                .create("productSpecification")
                .unwrap()
                .write_scalar(&VarLenAscii::from_ascii(&"X".repeat(256)).unwrap())
                .unwrap();
        }
        assert!(hdf_product(&text).unwrap_err().to_string().contains("255"));
        assert!(checked_product_text(b"INT.IHO.S-102.3.0.0\0hidden").is_err());
        assert!(checked_product_text(&[0xff]).is_err());
        assert!(checked_product_text(&[b'X'; 255]).is_ok());
    }
    #[test]
    fn empty_and_unknown_inputs_produce_visible_diagnostics_not_guessed_products() {
        let tmp = TestDir::new();
        assert_eq!(discover(tmp.path()).unwrap().notices.len(), 1);
        let p = tmp.path().join("unknown.xml");
        std::fs::write(&p, []).unwrap();
        assert!(single_file(&p)
            .unwrap_err()
            .to_string()
            .contains("Malformed S421/XML input"));
        std::fs::write(&p, b"<unrelated/>").unwrap();
        let plan = single_file(&p).unwrap();
        assert!(plan.charts.is_empty() && plan.rasters.is_empty() && plan.routes.is_empty());
        assert_eq!(plan.notices.len(), 1);
    }
    #[test]
    fn archive_only_folder_explains_extraction_without_guessing_products() {
        let tmp = TestDir::new();
        std::fs::create_dir(tmp.path().join("Data")).unwrap();
        std::fs::write(tmp.path().join("Data/exchange.zip"), b"not parsed").unwrap();
        let plan = discover(tmp.path()).unwrap();
        assert!(plan.rasters.is_empty() && plan.charts.is_empty());
        assert_eq!(plan.notices.len(), 1);
        assert!(plan.notices[0].contains("ZIP archives"));
        assert!(plan.notices[0].contains("S100_ROOT"));
    }
    #[cfg(unix)]
    #[test]
    fn recursion_does_not_follow_symlink_deliveries() {
        let tmp = TestDir::new();
        let outside = TestDir::new();
        std::fs::write(outside.path().join("CELL.000"), []).unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked")).unwrap();
        assert!(discover(tmp.path()).unwrap().charts.is_empty());
        assert!(discover(&tmp.path().join("linked")).is_err());
    }
    /// Actual files are discovered read-only; full DSID/authorization/planner
    /// qualification remains the existing production loader's signed probes.
    #[test]
    #[ignore = "requires local read-only official TestData"]
    fn actual_shom_and_ukho_nested_discovery_keeps_updates_original_paths() {
        let repo =
            std::path::Path::new("/Users/hoyeoncho/Desktop/Development/FerriteS100/TestData");
        for name in ["SHOM-S101", "UKHO-S101"] {
            let p = discover(&repo.join(name)).unwrap();
            assert!(!p.charts.is_empty());
            assert!(p.charts.iter().all(|v| v.starts_with(repo.join(name))));
            if name == "SHOM-S101" {
                assert!(p
                    .charts
                    .iter()
                    .any(|v| v.extension().is_some_and(|e| e != "000")));
            } else {
                assert_eq!(p.charts.len(), 17);
                assert!(p
                    .charts
                    .iter()
                    .all(|v| v.extension().is_some_and(|e| e == "000")));
            }
        }
    }
}
