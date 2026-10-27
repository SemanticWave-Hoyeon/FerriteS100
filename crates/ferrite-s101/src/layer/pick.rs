//! Feature reports of picked S-101 features: the feature of a loaded cell
//! described with the Feature Catalogue bound to that cell
//! ([`super::pick_fields`]).
use super::pick_fields;
use super::S101Layer;
use ferrite_product_api::report::FeatureReport;

impl S101Layer {
    /// Describe ENC feature `feature_id` of loaded cell `cell_index` with the
    /// Feature Catalogue bound to that cell. An index-free hit cannot
    /// identify an ENC feature: only `fallback_type` is reported.
    pub fn describe_feature(
        &self,
        cell_index: Option<u32>,
        feature_id: i64,
        fallback_type: &str,
    ) -> FeatureReport {
        let feature = cell_index.and_then(|cell_idx| {
            self.cells
                .get(cell_idx as usize)
                .and_then(|cell| cell.features.get(&feature_id))
        });
        let feature_code = feature
            .and_then(|f| f.feature_code.as_deref())
            .unwrap_or(fallback_type)
            .to_string();
        let source_fc = cell_index
            .and_then(|index| self.catalogue_owner_for_cell(index as usize))
            .map(|owner| {
                let fc: &ferrite_feature_catalog::FeatureCatalogue = owner.fc();
                fc
            });
        let fields = feature
            .map(|f| pick_fields::resolve(source_fc, &feature_code, &f.attributes))
            .unwrap_or_default();
        let definition = fields.resolved.then_some(fields.definition).flatten();
        FeatureReport {
            feature_type: feature_code,
            feature_id,
            foid: feature
                .and_then(|f| f.foid.as_ref())
                .map(ToString::to_string),
            cell_index,
            primitive_type: feature
                .map(|f| format!("{:?}", f.primitive_type))
                .unwrap_or_else(|| "Unknown".into()),
            source: cell_index
                .and_then(|i| self.cells.get(i as usize))
                .map(|c| c.file_path.display().to_string()),
            attributes: fields.attributes,
            definition,
        }
    }
}
