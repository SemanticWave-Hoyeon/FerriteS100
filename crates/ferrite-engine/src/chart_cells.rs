//! Loaded S-101 cells: data extents, canonical digests and catalogue
//! normalization rollback.
use anyhow::{Context, Result};
use ferrite_render::{GeoBounds, WorldPoint};
use ferrite_s100_core::S101Cell;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

/// Rebuild raw data extents after a cell replacement; raster tile bounds remain
/// part of the same mixed-product view. No temporary flattened coordinate arrays.
pub fn chart_data_bounds(
    cells: &[S101Cell],
    rasters: impl IntoIterator<Item = GeoBounds>,
) -> GeoBounds {
    let mut bounds = GeoBounds::default();
    for cell in cells {
        for point in cell.points.values() {
            bounds.expand(WorldPoint::new(point.position.x, point.position.y));
        }
        for points in cell.multi_points.values() {
            for pos in &points.positions {
                bounds.expand(WorldPoint::new(pos.x, pos.y));
            }
        }
        for curve in cell.curves.values() {
            for segment in &curve.segments {
                for pos in &segment.positions {
                    bounds.expand(WorldPoint::new(pos.x, pos.y));
                }
            }
        }
    }
    for raster in rasters {
        bounds.expand(WorldPoint::new(raster.min_x, raster.min_y));
        bounds.expand(WorldPoint::new(raster.max_x, raster.max_y));
    }
    bounds
}

pub const CANONICAL_CELL_PARTS: [&str; 10] = [
    "fields",
    "code_mappings",
    "points",
    "multi_points",
    "curves",
    "composite_curves",
    "surfaces",
    "features",
    "information",
    "spatial_information_associations",
];

/// Order-independent digests of every cell field (maps sorted by key), one
/// per `CANONICAL_CELL_PARTS` entry.
pub fn canonical_cell_digest(cell: &S101Cell) -> Vec<[u8; 32]> {
    fn sorted<K: Ord + std::fmt::Debug, V: std::fmt::Debug>(
        map: &std::collections::HashMap<K, V>,
    ) -> String {
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        format!("{entries:?}")
    }
    let mut shell = cell.clone();
    shell.release_records();
    shell.code_mappings = Default::default();
    let m = &cell.code_mappings;
    let mappings = [
        &m.attributes,
        &m.information_types,
        &m.feature_types,
        &m.information_associations,
        &m.feature_associations,
        &m.association_roles,
    ]
    .iter()
    .map(|c| sorted(&c.num_to_str) + &sorted(&c.str_to_num))
    .collect::<String>();
    [
        format!("{shell:?}"),
        mappings,
        sorted(&cell.points),
        sorted(&cell.multi_points),
        sorted(&cell.curves),
        sorted(&cell.composite_curves),
        sorted(&cell.surfaces),
        sorted(&cell.features),
        sorted(&cell.information),
        sorted(&cell.spatial_information_associations),
    ]
    .iter()
    .map(|part| Sha256::digest(part.as_bytes()).into())
    .collect()
}

/// Only the fields modified by normalize_feature_codes; graph, source and order
/// remain owned by the loaded cell. Used for synchronous preparation rollback.
pub struct CatalogueNormalization {
    feature_codes: Vec<(i64, Option<String>)>,
    mapping: ferrite_s100_core::CodeMapping,
}
impl CatalogueNormalization {
    pub fn capture(cell: &S101Cell) -> Self {
        Self {
            feature_codes: cell
                .features
                .iter()
                .map(|(id, f)| (*id, f.feature_code.clone()))
                .collect(),
            mapping: cell.code_mappings.feature_types.clone(),
        }
    }
    pub fn restore(self, cell: &mut S101Cell) {
        for (id, code) in self.feature_codes {
            cell.features
                .get_mut(&id)
                .expect("Preparation preserves feature ownership")
                .feature_code = code;
        }
        cell.code_mappings.feature_types = self.mapping;
    }
}
/// Load all chart data from directory
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

    // Find all .000 files (filter to specific file for testing)
    let chart_files: Vec<PathBuf> = WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().is_some_and(|ext| ext == "000")
                // Filter to specific file for testing
                && e.path().file_name().is_some_and(|name| name == "101GB00GB302045.000")
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    info!("Found {} chart files", chart_files.len());

    // Load each chart
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

#[cfg(test)]
mod updated_chart_bounds_tests {
    use super::*;
    #[test]
    fn retains_raster_extent_and_drops_deleted_vector_extent_without_margin_growth() {
        let path = std::env::temp_dir().join(format!("ferrite-bounds-{}.000", std::process::id()));
        std::fs::write(&path, b"000253LE1 0000025 ! 1104\x1e").unwrap();
        let (mut cell, _) = S101Cell::load_from_with_identity(&path, &path).unwrap();
        std::fs::remove_file(path).unwrap();
        let id = ferrite_s100_core::RecordId::new(110, 1);
        cell.points.insert(
            id.key(),
            ferrite_s100_core::PointRecord {
                id,
                position: ferrite_s100_core::Coordinate::new(70., 80.),
                update_instruction: 1,
            },
        );
        let raster = GeoBounds::new(-10., -20., 10., 20.);
        let cells = vec![cell];
        let before = chart_data_bounds(&cells, [raster]);
        assert_eq!(
            (before.min_x, before.min_y, before.max_x, before.max_y),
            (-10., -20., 70., 80.)
        );
        let mut cells = cells;
        cells[0].points.clear();
        let after = chart_data_bounds(&cells, [raster]);
        assert_eq!(
            (after.min_x, after.min_y, after.max_x, after.max_y),
            (-10., -20., 10., 20.)
        );
        let again = chart_data_bounds(&cells, [raster]);
        assert_eq!(
            (again.min_x, again.min_y, again.max_x, again.max_y),
            (-10., -20., 10., 20.)
        );
    }
}

#[cfg(test)]
mod manual_catalogue_normalization_tests {
    use super::*;
    use crate::{dataset_signature_policy, s101_update_plan};
    fn cell() -> S101Cell {
        S101Cell {
            file_path: PathBuf::from("original.000"),
            dsid: Default::default(),
            code_mappings: Default::default(),
            coord_factor: 1.,
            coord_factor_y: 1.,
            coord_factor_z: 1.,
            coord_origin_x: 0.,
            coord_origin_y: 0.,
            coord_origin_z: 0.,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: Default::default(),
            multi_points: Default::default(),
            curves: Default::default(),
            composite_curves: Default::default(),
            surfaces: Default::default(),
            features: Default::default(),
            information: Default::default(),
            spatial_information_associations: Default::default(),
        }
    }
    #[test]
    fn canonical_cell_digest_ignores_map_order_and_detects_changes() {
        let mut a = cell();
        let mut b = cell();
        for code in 0..64u16 {
            a.code_mappings
                .feature_types
                .insert(code, format!("F{code}"));
            b.code_mappings
                .feature_types
                .insert(63 - code, format!("F{}", 63 - code));
        }
        assert_eq!(canonical_cell_digest(&a), canonical_cell_digest(&b));
        b.code_mappings.feature_types.insert(5, "Changed".into());
        assert_ne!(canonical_cell_digest(&a), canonical_cell_digest(&b));
        a.normalize_feature_codes(&["F1".into()]);
        assert_ne!(canonical_cell_digest(&a), canonical_cell_digest(&b));
    }
    /// A released cell re-materialized from its plan, with the same code
    /// normalizations, equals the cell as originally loaded.
    #[test]
    fn released_cell_rematerializes_identically() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(
            "TestData/UKHO-S100-2026/ExchangeSets/S-101_GB_Apr26/S100_ROOT/S-101/DATASET_FILES/101GB004DEVQK.000",
        );
        if !path.exists() {
            eprintln!("skipped: {} not present", path.display());
            return;
        }
        let candidates = s101_update_plan::expand_candidates(&[path], &[]).unwrap();
        let authorization = dataset_signature_policy::unchecked_datasets(&candidates).unwrap();
        let plan = s101_update_plan::authorized_plans(candidates, &authorization, false)
            .unwrap()
            .remove(0);
        let codes = vec!["CardinalBuoy".to_string(), "LateralBuoy".to_string()];
        let (mut original, identity) = plan.load().unwrap();
        original.normalize_feature_codes(&codes);
        let expected = canonical_cell_digest(&original);
        let stats = original.statistics();
        assert!(stats.features > 0);
        assert_eq!(original.release_records(), stats);
        assert!(original.features.is_empty() && original.curves.is_empty());
        let (mut again, again_identity) = plan.load().unwrap();
        assert_eq!(identity.sha256(), again_identity.sha256());
        plan.validate_materialized(&again).unwrap();
        again.normalize_feature_codes(&codes);
        assert_eq!(canonical_cell_digest(&again), expected);
    }
    #[test]
    fn normalization_rollback_restores_both_mapping_directions_and_source() {
        let mut cell = cell();
        cell.code_mappings
            .feature_types
            .insert(7, "BuoyCardinal".into());
        // Keep an unrelated reverse entry: normalization is not a canonical-map rebuild.
        cell.code_mappings
            .feature_types
            .str_to_num
            .insert("OldAlias".into(), 9);
        let original = CatalogueNormalization::capture(&cell);
        cell.normalize_feature_codes(&["CardinalBuoy".into()]);
        assert_eq!(
            cell.code_mappings.feature_types.get_string(7).unwrap(),
            "CardinalBuoy"
        );
        original.restore(&mut cell);
        assert_eq!(
            cell.code_mappings.feature_types.get_string(7).unwrap(),
            "BuoyCardinal"
        );
        assert_eq!(
            cell.code_mappings.feature_types.get_numeric("OldAlias"),
            Some(9)
        );
        assert_eq!(cell.file_path, PathBuf::from("original.000"));
    }
    #[test]
    fn repeated_failed_preparations_restore_original_mapping_exactly() {
        let mut cell = cell();
        cell.code_mappings
            .feature_types
            .insert(7, "buoycardinal".into());
        let before = cell.code_mappings.feature_types.clone();
        for _ in 0..2 {
            let original = CatalogueNormalization::capture(&cell);
            cell.normalize_feature_codes(&["BuoyCardinal".into()]);
            original.restore(&mut cell);
            assert_eq!(
                cell.code_mappings.feature_types.num_to_str,
                before.num_to_str
            );
            assert_eq!(
                cell.code_mappings.feature_types.str_to_num,
                before.str_to_num
            );
        }
    }
    #[test]
    fn rollback_restores_none_and_alias_feature_codes_without_replacing_graph() {
        let mut cell = cell();
        for (id, code) in [(1, Some("BuoyCardinal")), (2, None)] {
            cell.features.insert(
                id,
                ferrite_s100_core::FeatureRecord {
                    frid: ferrite_s100_core::FRID {
                        rcid: id as u32,
                        nftc: 7,
                        rver: 3,
                        ruin: 1,
                    },
                    foid: None,
                    attributes: Vec::new(),
                    spatial_associations: Vec::new(),
                    information_associations: Vec::new(),
                    feature_associations: Vec::new(),
                    masks: Vec::new(),
                    feature_code: code.map(str::to_owned),
                    primitive_type: ferrite_s100_core::SpatialPrimitiveType::NoGeometry,
                },
            );
        }
        let original = CatalogueNormalization::capture(&cell);
        cell.normalize_feature_codes(&["CardinalBuoy".into()]);
        assert_eq!(
            cell.features[&1].feature_code.as_deref(),
            Some("CardinalBuoy")
        );
        original.restore(&mut cell);
        assert_eq!(
            cell.features[&1].feature_code.as_deref(),
            Some("BuoyCardinal")
        );
        assert_eq!(cell.features[&2].feature_code, None);
        assert_eq!(cell.features[&1].frid.rver, 3);
        assert_eq!(cell.features.len(), 2);
    }
}
