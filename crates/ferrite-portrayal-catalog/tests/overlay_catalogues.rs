//! S-124 / S-129 overlay portrayal catalogues load through the same APIs the
//! S-101 conversion uses (`line_styles`, `color_profiles.profiles[..].get_srgb`).
use ferrite_portrayal_catalog::*;
use std::path::PathBuf;

fn pc_dir(product: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../Catalogues/PC")
        .join(product)
}

fn load(product: &str) -> Option<PortrayalCatalogue> {
    let dir = pc_dir(product);
    if !dir.join("portrayal_catalogue.xml").exists() {
        eprintln!("skipping: {} not present", dir.display());
        return None;
    }
    Some(PortrayalCatalogue::load(dir).expect("overlay PC loads"))
}

/// Mirrors `ferrite_s101::lookup_pc_color` profile selection.
fn color(pc: &PortrayalCatalogue, profile: &str, token: &str) -> Option<SrgbColor> {
    pc.color_profiles
        .profiles
        .get(profile)
        .or_else(|| pc.color_profiles.get_default())
        .and_then(|p| p.get_srgb(token))
}

fn simple<'a>(pc: &'a PortrayalCatalogue, id: &str) -> &'a SimpleLineStyle {
    match pc
        .line_styles
        .get(id)
        .unwrap_or_else(|| panic!("line style {id}"))
    {
        LineStyle::Simple(s) => s,
        other => panic!("{id} not simple: {other:?}"),
    }
}

#[test]
fn s124_catalogue_loads() {
    let Some(pc) = load("S-124") else { return };
    assert_eq!(pc.product_id, "S-124");
    for profile in ["Day", "Dusk", "Night"] {
        assert!(color(&pc, profile, "CHMGD").is_some(), "{profile} CHMGD");
    }
    assert_eq!(pc.color_profiles.default_profile.as_deref(), Some("Day"));
    for symbol in [
        "NavigationalWarningFeaturePart",
        "NavigationalWarningFeaturePartPoint",
        "ApproximatePosition",
        "QUESMRK1",
        "EMQUESM1",
    ] {
        assert!(pc.get_symbol(symbol).is_some(), "symbol {symbol}");
        assert!(pc.symbols.get_svg_path(symbol).unwrap().exists());
    }
    let nw = simple(&pc, "NavigationalWarningFeaturePartLine");
    assert_eq!(nw.pen.color_token, "CHMGD");
    assert_eq!(nw.dashes.len(), 5);
    assert_eq!(
        nw.symbols[0].reference,
        "NavigationalWarningFeaturePartPoint"
    );
    assert!(nw.dash_cycle().unwrap().is_some());
    let q = simple(&pc, "QUESMRK1");
    assert_eq!(q.symbols[0].reference, "EMQUESM1");
    assert!(pc.get_area_fill("QUESMRK1").is_some());
    for vg in [24, 31020, 21010] {
        assert!(pc.viewing_groups.groups.contains_key(&vg), "vg {vg}");
    }
    assert_eq!(pc.display_planes.resolve("OverRADAR").unwrap().get(), 1);
    assert_eq!(pc.display_planes.resolve("UnderRADAR").unwrap().get(), -1);

    assert_eq!(pc.rule_type(), Some(RuleType::Xslt));
    assert_eq!(
        pc.rules.main_file.as_deref(),
        Some(std::path::Path::new("main.xsl"))
    );
    let main = pc.main_rule_path().unwrap();
    assert!(main.ends_with("Rules/main.xsl") && main.exists());
    let rule = pc.rules.main_rule().unwrap();
    assert!(rule.top_level && rule.rule_type == RuleType::Xslt && rule.id == "main");
    let sub = pc.rules.get_rule("NavwarnPart").unwrap();
    assert!(!sub.top_level && sub.rule_type == RuleType::Xslt);
    assert!(pc.rules.get_rule_path("NavwarnPart").unwrap().exists());
}

#[test]
fn s129_catalogue_loads() {
    let Some(pc) = load("S-129") else { return };
    assert_eq!(pc.product_id, "S-129");
    for profile in ["Day", "Dusk", "Night"] {
        for token in ["RED", "GOLDN", "PLRTE", "BLK", "WHITE", "CHMGD"] {
            assert!(color(&pc, profile, token).is_some(), "{profile} {token}");
        }
    }
    // Unknown palette names fall back to the default (Day) palette.
    assert!(color(&pc, "DayBright", "RED").is_some());
    for symbol in ["DIAMOND1P", "UKCCONPT", "EMUKCARE", "EMAREMG1"] {
        assert!(pc.get_symbol(symbol).is_some(), "symbol {symbol}");
    }
    let ukc = simple(&pc, "UKCARE01");
    assert_eq!(ukc.pen.color_token, "CHMGD");
    assert!((ukc.interval_length - 49.).abs() < 1e-9);
    assert_eq!(ukc.symbols.len(), 6);
    assert!(ukc.dash_cycle().unwrap().is_some());
    for vg in [29010, 29020, 29030, 29040, 29050, 29060, 29070] {
        assert!(pc.viewing_groups.groups.contains_key(&vg), "vg {vg}");
    }
    assert_eq!(pc.display_planes.resolve("UnderRadar").unwrap().get(), -1);
    let plain = pc.get_context_parameter("PlainBoundaries").unwrap();
    assert_eq!(plain.param_type, ContextParamType::Boolean);
    assert_eq!(plain.default_value.as_deref(), Some("true"));

    assert_eq!(pc.rule_type(), Some(RuleType::Xslt));
    let main = pc.main_rule_path().unwrap();
    assert!(main.ends_with("Rules/main.xsl") && main.exists());
}

#[test]
fn namespacing_rewrites_embedded_symbol_references() {
    let Some(mut pc) = load("S-129") else { return };
    pc.namespace_symbol_references("S-129");
    pc.namespace_symbol_references("S-129"); // idempotent
    let ukc = simple(&pc, "UKCARE01");
    assert!(ukc
        .symbols
        .iter()
        .all(|s| s.reference.starts_with("S-129/") && s.reference.matches('/').count() == 1));
    assert_eq!(
        split_overlay_symbol_id(&ukc.symbols[5].reference),
        Some(("S-129", "EMUKCARE"))
    );
    // Symbol table keeps plain catalogue ids.
    assert!(pc.get_symbol("EMUKCARE").is_some());

    let Some(mut s124) = load("S-124") else {
        return;
    };
    s124.namespace_symbol_references("S-124");
    match &s124.get_area_fill("QUESMRK1").unwrap().fill_type {
        AreaFillType::Symbol(s) => assert!(s.symbol_ref.starts_with("S-124/")),
        AreaFillType::Pattern(p) => assert!(p.symbol_ref.starts_with("S-124/")),
        _ => {}
    }
}

#[test]
fn s101_rules_remain_lua() {
    let Some(pc) = load("S-101") else { return };
    assert_eq!(pc.rule_type(), Some(RuleType::Lua));
    assert_eq!(
        pc.rules.main_file.as_deref(),
        Some(std::path::Path::new("main.lua"))
    );
}
