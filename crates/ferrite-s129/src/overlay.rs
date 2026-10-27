//! The S-129 overlay module: the PlainBoundaries context parameter, the
//! feature `scaleMinimum` option, the virtual input features and the
//! catalogue errata the "Correct known PC errata" option corrects.
use crate::rules::S129;
use ferrite_product_api::{
    Encoding, PanelDescriptor, ProductAdapter, ProductInfo, Recognised, SceneRole,
};
use ferrite_s100_gml::portrayal::{CatalogueInfo, ConvertOptions, InputOptions};
use ferrite_s100_gml::product::{GmlOverlayProduct, GmlRules, PortrayalSettings};
use ferrite_s100_gml::Product;
use std::any::Any;
use std::collections::HashSet;
use std::path::Path;
use tracing::warn;

/// The S-129 overlay module.
pub struct S129Overlay {
    /// S-129 context parameter PlainBoundaries (PC default).
    pub plain_boundaries: bool,
    /// Apply S-129 feature `scaleMinimum` (S-129 says it controls visibility).
    pub apply_scale_minimum: bool,
}

impl Default for S129Overlay {
    fn default() -> Self {
        Self {
            plain_boundaries: true,
            apply_scale_minimum: true,
        }
    }
}

impl ProductAdapter for S129Overlay {
    fn info(&self) -> ProductInfo {
        ProductInfo {
            id: crate::PRODUCT,
            title: "Under Keel Clearance Management",
            encoding: Encoding::Gml,
            role: SceneRole::Overlay,
        }
    }
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised> {
        ferrite_s100_gml::overlay::sniff(name, head, &[Product::S129]).map(|_| Recognised::Dataset)
    }
    fn panels(&self) -> &'static [PanelDescriptor] {
        std::slice::from_ref(&crate::panel::PANEL)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl GmlOverlayProduct for S129Overlay {
    fn rules(&self) -> &'static dyn GmlRules {
        &S129
    }

    fn catalogues_loaded(&mut self, info: &CatalogueInfo, pc_version: &str) {
        self.plain_boundaries = info
            .context
            .iter()
            .find(|(k, _)| k == "PlainBoundaries")
            .map(|(_, v)| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(true);
        warn!(
            "Conformance note: S-129 {pc_version} InformationBox rule sets text transparency 1, which is fully transparent by S-100 Part 9 9-12.2.2.3; the text is not visible. \"Correct known PC errata\" draws it opaque"
        );
    }

    fn errata_correction(&self) -> Option<&'static str> {
        Some("S-129 transparency-1 text drawn opaque")
    }

    fn input_options(&self) -> InputOptions {
        crate::virtual_features::input_options()
    }

    fn portrayal_settings(
        &self,
        errata: bool,
        _implicit_groups: &HashSet<String>,
    ) -> PortrayalSettings {
        let plain = if self.plain_boundaries {
            "true"
        } else {
            "false"
        };
        PortrayalSettings {
            key: self.plain_boundaries as u64
                | (self.apply_scale_minimum as u64) << 1
                | (errata as u64) << 2,
            context: vec![("PlainBoundaries", plain)],
            convert: ConvertOptions {
                apply_feature_scale_minimum: self.apply_scale_minimum,
                opaque_invisible_text: errata,
                ..ConvertOptions::default()
            },
        }
    }
}
