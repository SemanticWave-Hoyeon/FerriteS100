//! Product-derived independent mariner selector; unknown contracts retain portrayal.
use crate::{AreaFillType, DisplayPlane, DrawingInstruction, PatternCrs, PortrayalOrigin};
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use sha2::{Digest, Sha256};
#[derive(Debug, Clone)]
pub struct ShallowPatternContract {
    digest: [u8; 32],
    group: u32,
    fill_reference: String,
    symbol: String,
    v1: (f32, f32),
    v2: (f32, f32),
    crs: PatternCrs,
    priority: i32,
    plane: DisplayPlane,
}
impl ShallowPatternContract {
    /// Exact independent selector contract identity; sharing a resource cache
    /// must not merge different optionality/provenance permissions.
    pub fn same_bound_contract(&self, other: &Self) -> bool {
        self.digest == other.digest
            && self.group == other.group
            && self.fill_reference == other.fill_reference
            && self.symbol == other.symbol
            && self.crs == other.crs
            && self.priority == other.priority
            && self.plane == other.plane
            && self.v1.0.to_bits() == other.v1.0.to_bits()
            && self.v1.1.to_bits() == other.v1.1.to_bits()
            && self.v2.0.to_bits() == other.v2.0.to_bits()
            && self.v2.1.to_bits() == other.v2.1.to_bits()
    }

    /// Bind a product adapter's independently audited selector profile to exact
    /// immutable PC rules, selector group and resolved SymbolFill resource.
    /// This checks provenance/shape; the adapter owns product optionality semantics.
    #[expect(
        clippy::too_many_arguments,
        reason = "Catalogue ownership, audited rule digest, selector layer/group, resolved fill, drawing priority and plane are independently validated provenance inputs; preserve product adapter API."
    )]
    pub fn from_bound_selector(
        pc: &BoundPortrayalCatalogue,
        rule_path: &std::path::Path,
        expected_rule_sha: [u8; 32],
        layer_id: &str,
        group_id: &str,
        fill_reference: &str,
        priority: i32,
        plane: DisplayPlane,
    ) -> Option<Self> {
        let source = pc.sources();
        let rule = source.read_relative(rule_path).ok()?;
        if <[u8; 32]>::from(Sha256::digest(rule.as_ref())) != expected_rule_sha {
            return None;
        }
        let group = pc.viewing_groups.runtime_id(group_id)?;
        let layer = pc.viewing_group_layers.layers.get(layer_id)?;
        if layer.viewing_group_ids.as_slice() != [group] {
            return None;
        }
        let fill = pc.get_area_fill(fill_reference)?;
        let ferrite_portrayal_catalog::AreaFillType::Symbol(symbol) = &fill.fill_type else {
            return None;
        };
        let crs = symbol.area_crs.parse::<PatternCrs>().ok()?;
        let v1 = (symbol.v1.x as f32, symbol.v1.y as f32);
        let v2 = (symbol.v2.x as f32, symbol.v2.y as f32);
        if [v1.0, v1.1, v2.0, v2.1].iter().any(|v| !v.is_finite()) {
            return None;
        }
        if v1.0 as f64 * v2.1 as f64 - v1.1 as f64 * v2.0 as f64 == 0. {
            return None;
        }
        Some(Self {
            digest: *pc.source_digest(),
            group,
            fill_reference: fill_reference.into(),
            symbol: symbol.symbol_ref.clone(),
            v1,
            v2,
            crs,
            priority,
            plane,
        })
    }
    pub fn source_digest(&self) -> [u8; 32] {
        self.digest
    }
    pub fn is_optional(&self, instruction: &DrawingInstruction) -> bool {
        let DrawingInstruction::Area(area) = instruction else {
            return false;
        };
        let AreaFillType::Pattern { symbol_ref, v1, v2 } = &area.fill else {
            return false;
        };
        area.fill_ref.as_deref() == Some(self.fill_reference.as_str())
            && symbol_ref == &self.symbol
            && v1.0.to_bits() == self.v1.0.to_bits()
            && v1.1.to_bits() == self.v1.1.to_bits()
            && v2.0.to_bits() == self.v2.0.to_bits()
            && v2.1.to_bits() == self.v2.1.to_bits()
            && area.pattern_crs == self.crs
            && area.viewing_group.0 == self.group
            && area.additional_viewing_groups.is_empty()
            && area.priority.0 == self.priority
            && area.display_plane == self.plane
            && area.feature_id.is_some()
            && area.cell_index.is_some()
            && matches!(area.portrayal_origin, PortrayalOrigin::NonPoint)
    }
}
pub fn pattern_display_allows(
    instruction: &DrawingInstruction,
    show: bool,
    contract: Option<&ShallowPatternContract>,
) -> bool {
    show || !contract.is_some_and(|contract| contract.is_optional(instruction))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AreaInstruction, WorldPoint};
    fn contract() -> ShallowPatternContract {
        ShallowPatternContract {
            digest: [7; 32],
            group: 90000,
            fill_reference: "DIAMOND1".into(),
            symbol: "DIAMOND1P".into(),
            v1: (22.5, 0.),
            v2: (0., 43.13),
            crs: PatternCrs::GlobalGeometry,
            priority: 9,
            plane: DisplayPlane::UnderRadar,
        }
    }
    #[test]
    fn sharing_requires_complete_exact_selector_permission() {
        let a = contract();
        assert!(a.same_bound_contract(&a.clone()));
        let mut b = a.clone();
        b.v1.1 = -0.;
        assert!(!a.same_bound_contract(&b));
        let mut b = a.clone();
        b.digest[0] ^= 1;
        assert!(!a.same_bound_contract(&b));
        let mut b = a.clone();
        b.group += 1;
        assert!(!a.same_bound_contract(&b));
    }
    fn area(fill: &str, symbol: &str, group: u32) -> DrawingInstruction {
        let mut area = AreaInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 0.),
            WorldPoint::new(0., 1.),
        ])
        .with_pattern_fill(symbol.into(), (22.5, 0.), (0., 43.13))
        .with_priority(9)
        .with_feature_id(100)
        .with_cell_index(2)
        .with_pattern_crs(PatternCrs::GlobalGeometry);
        area.fill_ref = Some(fill.into());
        area.viewing_group.0 = group;
        area.portrayal_origin = PortrayalOrigin::NonPoint;
        DrawingInstruction::Area(area)
    }
    #[test]
    fn optional_selector_does_not_hide_dredged_restriction_or_unknown_patterns() {
        let c = contract();
        let commands = vec![
            area("DIAMOND1", "DIAMOND1P", 90000),
            area("DRGARE01", "DRGARE01P", 13030),
            area("TSSJCT02", "TSSJCT02P", 25010),
            area("NODATA03", "NODATA03P", 11050),
            area("DIAMOND1", "DIAMOND1P", 90010),
        ];
        let raw = serde_json::to_vec(&commands).unwrap();
        let visible = |show| {
            commands
                .iter()
                .enumerate()
                .filter_map(|(i, item)| pattern_display_allows(item, show, Some(&c)).then_some(i))
                .collect::<Vec<_>>()
        };
        assert_eq!(visible(false), vec![1, 2, 3, 4]);
        assert_eq!(visible(true), vec![0, 1, 2, 3, 4]);
        assert_eq!(visible(false), vec![1, 2, 3, 4]);
        assert_eq!(serde_json::to_vec(&commands).unwrap(), raw);
        assert!(commands
            .iter()
            .all(|i| pattern_display_allows(i, false, None)));
    }
    #[test]
    fn mismatched_source_style_lattice_and_overlay_are_retained() {
        let c = contract();
        let DrawingInstruction::Area(original) = area("DIAMOND1", "DIAMOND1P", 90000) else {
            unreachable!()
        };
        for change in 0..5 {
            let mut a = original.clone();
            match change {
                0 => a.fill_ref = None,
                1 => a.cell_index = None,
                2 => a.portrayal_origin = PortrayalOrigin::CoverageExempt,
                3 => a.priority.0 = 3,
                _ => {
                    if let AreaFillType::Pattern { v2, .. } = &mut a.fill {
                        v2.1 = f32::from_bits(v2.1.to_bits() + 1);
                    }
                }
            };
            assert!(pattern_display_allows(
                &DrawingInstruction::Area(a),
                false,
                Some(&c)
            ));
        }
    }
}
