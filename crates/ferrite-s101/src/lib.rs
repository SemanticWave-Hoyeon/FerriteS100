//! S-101 product adapter: the product layer, cancellations, coverage,
//! interoperability and catalogue checks of S-101 cells. The Part 9a
//! instruction conversion of cell portrayal is product-neutral
//! (`ferrite-part9a`).
pub mod cancellation;
pub mod layer;

mod pick_report;
pub use pick_report::pick_report_attributes;

mod interoperability;
pub use interoperability::{
    plan_interoperability, plan_interoperability_for_cells, plan_interoperability_for_cells_in,
    plan_interoperability_in, InteroperabilityPlan, ProductCoverage,
};

mod catalogue_compatibility;
pub use catalogue_compatibility::{validate_catalogue_pair, validate_dataset_catalogues};

mod spatial_scale_admission;
pub use spatial_scale_admission::validate_spatial_scale_properties;

mod context_validation;
pub use context_validation::{
    context_validation_parameters, synchronize_legacy_context, validate_portrayal_context,
};

mod display_selection;
pub use display_selection::{
    optional_viewing_layers, resolve_display_mode, viewing_groups_for_layers,
    viewing_groups_for_preset, DisplayPreset,
};

pub mod coverage_scale;

pub mod coverage_geometry;
pub mod coverage_loading;
pub mod coverage_projection;

// Optionality is a product-profile property, not implied by a rule hash or
// fill style. A custom catalogue can keep the same script while making this
// group foundational or adding it to a display-mode layer. Unknown semantics
// must leave the original commands visible.
fn shallow_selector_is_independent(pc: &ferrite_portrayal_catalog::PortrayalCatalogue) -> bool {
    if pc.product_id != "S-101" {
        return false;
    }
    let Some(group) = pc.viewing_groups.runtime_id("90000") else {
        return false;
    };
    if pc.foundation_mode.contains(&group) {
        return false;
    }
    let Some(layer) = pc.viewing_group_layers.layers.get("900") else {
        return false;
    };
    if layer.viewing_group_ids.as_slice() != [group] || pc.display_modes.modes.is_empty() {
        return false;
    }
    // Reject alternate membership as unknown even if it is currently omitted
    // by every mode: this is no longer the audited independent selector shape.
    if pc
        .viewing_group_layers
        .layers
        .iter()
        .any(|(id, layer)| id != "900" && layer.viewing_group_ids.contains(&group))
    {
        return false;
    }
    !pc.display_modes
        .modes
        .values()
        .any(|mode| mode.viewing_group_layers.iter().any(|id| id == "900"))
}

/// Current audited S-101 independent shallow-water selector profile. Unknown PC
/// rules retain every pattern; the host must report that this toggle is unavailable.
pub fn shallow_pattern_contract(
    pc: &ferrite_portrayal_catalog::BoundPortrayalCatalogue,
) -> Option<ferrite_render::ShallowPatternContract> {
    if !shallow_selector_is_independent(pc) {
        return None;
    }
    let known = [
        0x0f, 0xe2, 0x04, 0xc6, 0x15, 0x7b, 0xeb, 0xd6, 0x1a, 0xf1, 0xd5, 0x1f, 0x98, 0x16, 0xb1,
        0x36, 0x68, 0x3a, 0x1f, 0xad, 0x7c, 0xe7, 0x20, 0x03, 0x2f, 0xd9, 0x94, 0x29, 0x3a, 0x53,
        0xae, 0x9b,
    ];
    let fill = pc.get_area_fill("DIAMOND1")?;
    let ferrite_portrayal_catalog::AreaFillType::Symbol(s) = &fill.fill_type else {
        return None;
    };
    if s.symbol_ref != "DIAMOND1P"
        || s.area_crs != "GlobalGeometry"
        || (s.v1.x, s.v1.y) != (22.5, 0.)
        || (s.v2.x, s.v2.y) != (0., 43.13)
    {
        return None;
    }
    ferrite_render::ShallowPatternContract::from_bound_selector(
        pc,
        std::path::Path::new("Rules/SEABED01.lua"),
        known,
        "900",
        "90000",
        "DIAMOND1",
        9,
        ferrite_render::DisplayPlane::UnderRadar,
    )
}

#[cfg(test)]
mod shallow_pattern_catalogue_tests {
    #[test]
    fn parsed_snapshot_requires_independent_nonfoundation_selector() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-shallow-independence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let xml = |foundation: &str, mode: &str, extra: &str| {
            format!(
            "<portrayalCatalog productId='S-101' version='2.0.0'><foundationMode>{foundation}</foundationMode><displayPlanes><displayPlane id='UnderRadar' order='-1'/></displayPlanes><viewingGroups><viewingGroup id='90000'><description><name>Shallow pattern</name></description></viewingGroup></viewingGroups><viewingGroupLayers><viewingGroupLayer id='900'><viewingGroup>90000</viewingGroup></viewingGroupLayer>{extra}</viewingGroupLayers><displayModes><displayMode id='StandardDisplay'><name>Standard</name>{mode}</displayMode></displayModes></portrayalCatalog>")
        };
        let original = xml("", "", "");
        std::fs::write(root.join("portrayal_catalogue.xml"), &original).unwrap();
        let retained = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
        assert!(super::shallow_selector_is_independent(&retained));
        for mutated in [xml("<viewingGroup>90000</viewingGroup>","",""),
            xml("","<viewingGroupLayer>900</viewingGroupLayer>",""),
            xml("","","<viewingGroupLayer id='custom'><viewingGroup>90000</viewingGroup></viewingGroupLayer>")] {
            std::fs::write(root.join("portrayal_catalogue.xml"),mutated).unwrap();
            let pc=ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
            assert_ne!(retained.source_digest(),pc.source_digest());
            assert!(!super::shallow_selector_is_independent(&pc));
            assert!(super::shallow_pattern_contract(&pc).is_none());
        }
        assert!(super::shallow_selector_is_independent(&retained));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires immutable official PC input via FERRITE_SHALLOW_PC; mutates owned copy only"]
    fn official_same_rule_fill_and_layer_cannot_override_foundation_or_display_modes() {
        let source = std::path::PathBuf::from(std::env::var("FERRITE_SHALLOW_PC").unwrap());
        let original = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&source).unwrap();
        assert!(super::shallow_pattern_contract(&original).is_some());
        let root = std::env::temp_dir().join(format!(
            "ferrite-shallow-official-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // All bytes come from the bounded retained map, not a second live read.
        let mut pending = vec![source.clone()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                assert!(!kind.is_symlink());
                let path = entry.path();
                let relative = path.strip_prefix(&source).unwrap();
                let target = root.join(relative);
                if kind.is_dir() {
                    std::fs::create_dir_all(target).unwrap();
                    pending.push(path);
                } else {
                    assert!(kind.is_file());
                    let bytes = original.sources().read_relative(relative).unwrap();
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::write(target, bytes.as_ref()).unwrap();
                }
            }
        }
        let file = root.join("portrayal_catalogue.xml");
        let xml = std::fs::read_to_string(&file).unwrap();
        let copied = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
        assert!(super::shallow_pattern_contract(&copied).is_some());
        for (closing, insertion) in [
            ("</foundationMode>", "<viewingGroup>90000</viewingGroup>"),
            (
                "</displayMode>",
                "<viewingGroupLayer>900</viewingGroupLayer>",
            ),
        ] {
            assert!(xml.contains(closing));
            let mutant = xml.replacen(closing, &format!("{insertion}{closing}"), 1);
            std::fs::write(&file, mutant).unwrap();
            let parsed = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
            assert_eq!(
                copied
                    .sources()
                    .read_relative(std::path::Path::new("Rules/SEABED01.lua"))
                    .unwrap(),
                parsed
                    .sources()
                    .read_relative(std::path::Path::new("Rules/SEABED01.lua"))
                    .unwrap()
            );
            assert_eq!(
                parsed.viewing_group_layers.layers["900"].viewing_group_ids,
                copied.viewing_group_layers.layers["900"].viewing_group_ids
            );
            assert!(super::shallow_pattern_contract(&parsed).is_none());
            assert!(super::shallow_pattern_contract(&copied).is_some());
        }
        let after = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&source).unwrap();
        assert_eq!(original.source_digest(), after.source_digest());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires separately source-SHA-guarded official PC directory via FERRITE_SHALLOW_PC"]
    fn official_pc_selector_is_bound_and_nonshallow_patterns_are_retained() {
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(
            std::env::var("FERRITE_SHALLOW_PC").unwrap(),
        )
        .unwrap();
        let contract = super::shallow_pattern_contract(&pc).expect("audited selector contract");
        assert_eq!(contract.source_digest(), *pc.source_digest());
        let raw = super::shallow_pattern_contract(&pc).unwrap();
        for (fill, symbol, group, optional) in [
            ("DIAMOND1", "DIAMOND1P", 90000, true),
            ("DRGARE01", "DRGARE01P", 13030, false),
            ("NODATA03", "NODATA03P", 11050, false),
            ("TSSJCT02", "TSSJCT02P", 25010, false),
        ] {
            let mut a = ferrite_render::AreaInstruction::new(vec![
                ferrite_render::WorldPoint::new(0., 0.),
                ferrite_render::WorldPoint::new(1., 0.),
                ferrite_render::WorldPoint::new(0., 1.),
            ])
            .with_pattern_fill(symbol.into(), (22.5, 0.), (0., 43.13))
            .with_priority(9)
            .with_feature_id(1)
            .with_cell_index(0)
            .with_pattern_crs(ferrite_render::PatternCrs::GlobalGeometry);
            a.fill_ref = Some(fill.into());
            a.viewing_group.0 = group;
            a.portrayal_origin = ferrite_render::PortrayalOrigin::NonPoint;
            let item = ferrite_render::DrawingInstruction::Area(Box::new(a));
            assert_eq!(raw.is_optional(&item), optional);
            assert_eq!(
                ferrite_render::pattern_display_allows(&item, false, Some(&raw)),
                !optional
            );
        }
    }
}
