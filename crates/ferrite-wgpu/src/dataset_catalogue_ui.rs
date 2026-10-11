//! Application supplies exact source-cell catalogue metadata after publication.
//! This display helper never infers or substitutes a global catalogue owner.
use crate::egui_integration::{CatalogueStatus, DatasetLayerId, SelectedFeature};
#[derive(Debug, Clone)]
pub struct DatasetCatalogueBinding {
    pub cell_index: Option<u32>,
    pub id: DatasetLayerId,
    /// Actual S101Cell.file_path text, matching SelectedFeature.source.
    pub source: String,
    pub fc: CatalogueStatus,
    pub pc: CatalogueStatus,
}
fn unique<'a>(
    mut rows: impl Iterator<Item = &'a DatasetCatalogueBinding>,
) -> Option<&'a DatasetCatalogueBinding> {
    let first = rows.next()?;
    if rows.next().is_some() {
        None
    } else {
        Some(first)
    }
}
pub(crate) fn for_feature<'a>(
    rows: &'a [DatasetCatalogueBinding],
    feature: &SelectedFeature,
) -> Option<&'a DatasetCatalogueBinding> {
    let index = feature.cell_index?;
    let source = feature.source.as_deref()?;
    let owner = unique(rows.iter().filter(|r| r.cell_index == Some(index)))?;
    (owner.source == source).then_some(owner)
}
pub(crate) fn for_dataset<'a>(
    rows: &'a [DatasetCatalogueBinding],
    id: &DatasetLayerId,
) -> Option<&'a DatasetCatalogueBinding> {
    unique(rows.iter().filter(|r| &r.id == id))
}
/// The object inspector has a vertical scroll area, so full paths can wrap.
/// The tree retains its compact presentation; both use the same exact owner.
pub(crate) fn draw_inspector(ui: &mut egui::Ui, owner: Option<&DatasetCatalogueBinding>) {
    let Some(owner) = owner else {
        ui.weak("Dataset catalogue owner unavailable");
        return;
    };
    for (kind, status) in [("FC", &owner.fc), ("PC", &owner.pc)] {
        let version = if status.loaded {
            status.version.as_str()
        } else {
            "Unavailable"
        };
        crate::object_details::row(ui, &format!("{kind} version"), version);
        if !status.path.is_empty() {
            crate::object_details::row(ui, &format!("{kind} source"), &status.path);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn binding(index: u32, name: &str, version: &str) -> DatasetCatalogueBinding {
        DatasetCatalogueBinding {
            cell_index: Some(index),
            id: DatasetLayerId::S101 {
                product: "S-101".into(),
                name: name.into(),
            },
            source: format!("/data/{name}.000"),
            fc: CatalogueStatus {
                loaded: true,
                version: version.into(),
                path: format!("/fc/{version}.xml"),
                ..Default::default()
            },
            pc: CatalogueStatus {
                loaded: true,
                version: version.into(),
                path: format!("/pc/{version}"),
                ..Default::default()
            },
        }
    }
    fn selected(index: Option<u32>, source: Option<&str>) -> SelectedFeature {
        SelectedFeature {
            feature_type: "Wreck".into(),
            feature_id: 7,
            foid: None,
            cell_index: index,
            primitive_type: "Point".into(),
            source: source.map(str::to_owned),
            attributes: Vec::new(),
            world_pos: (0., 0.),
            longitude_shift: 0.,
            definition: None,
            symbol_name: None,
        }
    }
    #[test]
    fn same_record_id_in_mixed_cells_uses_exact_source_and_never_global() {
        let rows = vec![binding(0, "UKHO", "1.0.2"), binding(1, "SHOM", "2.0.0")];
        assert_eq!(
            for_feature(&rows, &selected(Some(0), Some("/data/UKHO.000")))
                .unwrap()
                .fc
                .version,
            "1.0.2"
        );
        assert_eq!(
            for_feature(&rows, &selected(Some(1), Some("/data/SHOM.000")))
                .unwrap()
                .fc
                .version,
            "2.0.0"
        );
        assert!(for_feature(&rows, &selected(Some(0), Some("/data/SHOM.000"))).is_none());
        assert!(for_feature(&rows, &selected(None, Some("/data/UKHO.000"))).is_none());
        let mut duplicate = rows.clone();
        duplicate.push(rows[0].clone());
        assert!(for_feature(&duplicate, &selected(Some(0), Some("/data/UKHO.000"))).is_none());
        assert!(for_dataset(&duplicate, &rows[0].id).is_none());
    }
    #[test]
    fn compaction_does_not_rebind_stale_selected_source() {
        let old = selected(Some(0), Some("/data/UKHO.000"));
        let compacted = vec![binding(0, "SHOM", "2.0.0")];
        assert!(for_feature(&compacted, &old).is_none());
    }
    #[test]
    fn native_route_catalogue_has_no_enc_cell_and_never_matches_feature_pick() {
        let mut native = binding(0, "ignored", "1.0-public");
        native.cell_index = None;
        native.id = DatasetLayerId::S421 { route_id: 7 };
        native.source = "/routes/original.gml".into();
        let rows = vec![native];
        assert!(for_dataset(&rows, &DatasetLayerId::S421 { route_id: 7 }).is_some());
        assert!(for_feature(&rows, &selected(Some(0), Some("/routes/original.gml"))).is_none());
    }
}
