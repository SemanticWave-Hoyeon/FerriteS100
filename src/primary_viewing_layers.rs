//! Host optional-layer intent scoped to one immutable captured S-101 PC.
//! Digest ownership is policy identity, not dataset authentication or authority.
use anyhow::{ensure, Result};
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_wgpu::SettingsState;
use std::collections::{BTreeSet, HashSet};
use std::hash::{Hash, Hasher};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PcLayerOwner([u8; 32]);

/// Borrowed typed projection of already-captured primary UI intent. IDs stay lexical.
struct PrimaryLayerPolicy<'a> {
    owner: Option<PcLayerOwner>,
    ids: &'a BTreeSet<String>,
}
impl<'a> PrimaryLayerPolicy<'a> {
    fn from_settings(settings: &'a SettingsState) -> Result<Self> {
        ensure!(settings.viewing_layers.is_empty() || settings.viewing_layer_owner.is_some(),
            "Optional S-101 layers have no captured primary PC owner; reselect them in the primary PC controls");
        Ok(Self {
            owner: settings.viewing_layer_owner.map(PcLayerOwner),
            ids: &settings.viewing_layers,
        })
    }
    fn resolve(&self, pc: &BoundPortrayalCatalogue) -> Result<HashSet<u32>> {
        ensure!(
            pc.product_id == "S-101",
            "S-101 layer selection requires S-101 PC"
        );
        if self.owner != Some(PcLayerOwner(*pc.source_digest())) {
            // There is no request for this PC. Do NOT filter unknown IDs or map names.
            return Ok(HashSet::new());
        }
        // Strict existing leaf validation still checks IDs and every referenced group.
        ferrite_s101::viewing_groups_for_layers(pc, self.ids.iter().map(String::as_str))
    }
}

/// Explicit startup/Apply capture. Validate first; mutate only the private candidate.
pub(crate) fn capture_primary(
    pc: &BoundPortrayalCatalogue,
    candidate: &mut SettingsState,
) -> Result<()> {
    ferrite_s101::viewing_groups_for_layers(
        pc,
        candidate.viewing_layers.iter().map(String::as_str),
    )?;
    candidate.viewing_layer_owner = Some(*pc.source_digest());
    Ok(())
}
pub(crate) fn resolve(
    pc: &BoundPortrayalCatalogue,
    settings: &SettingsState,
) -> Result<HashSet<u32>> {
    PrimaryLayerPolicy::from_settings(settings)?.resolve(pc)
}

/// A new primary catalogue gets no automatic optional selection inheritance.
/// The input is retained unchanged, so rejected candidate preparations preserve it.
pub(crate) fn for_new_primary(
    pc: &BoundPortrayalCatalogue,
    settings: &SettingsState,
) -> SettingsState {
    let mut next = settings.clone();
    if next.viewing_layer_owner != Some(*pc.source_digest()) {
        next.viewing_layers.clear();
        next.viewing_layer_owner = None;
    }
    next
}
pub(crate) fn validate_new_primary(
    pc: &BoundPortrayalCatalogue,
    settings: &SettingsState,
) -> Result<()> {
    resolve(pc, &for_new_primary(pc, settings)).map(|_| ())
}
pub(crate) fn hash_identity<H: Hasher>(settings: &SettingsState, hasher: &mut H) {
    "primary-S-101-optional-layer-policy-v1".hash(hasher);
    settings.viewing_layer_owner.hash(hasher);
    settings.viewing_layers.hash(hasher); // canonical BTreeSet lexical order
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(version: &str, optional: Option<u32>) -> (Self, BoundPortrayalCatalogue) {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "ferrite-owner-layer-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&p).unwrap();
            std::fs::create_dir(p.join("Rules")).unwrap();
            let group = optional
                .map(|v| format!("<viewingGroup id='{v}'/>"))
                .unwrap_or_default();
            let layer=optional.map(|v|format!("<viewingGroupLayer id='100'><viewingGroup>{v}</viewingGroup></viewingGroupLayer>")).unwrap_or_default();
            let xml=format!("<portrayalCatalog productId='S-101' version='{version}'><foundationMode><viewingGroup>1</viewingGroup></foundationMode><viewingGroups><viewingGroup id='1'/><viewingGroup id='2'/>{group}</viewingGroups><viewingGroupLayers><viewingGroupLayer id='base'><viewingGroup>2</viewingGroup></viewingGroupLayer>{layer}</viewingGroupLayers><displayModes><displayMode id='Standard'><viewingGroupLayer>base</viewingGroupLayer></displayMode></displayModes><displayPlanes><displayPlane id='OverRadar' order='1'/></displayPlanes></portrayalCatalog>");
            std::fs::write(p.join("portrayal_catalogue.xml"), xml).unwrap();
            std::fs::write(p.join("Rules/main.lua"), "return 'fixture'").unwrap();
            let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&p).unwrap();
            (Self(p), pc)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn selected(pc: &BoundPortrayalCatalogue) -> SettingsState {
        let mut s = SettingsState::default();
        s.viewing_layers.insert("100".into());
        capture_primary(pc, &mut s).unwrap();
        s
    }
    #[test]
    fn foreign_pc_has_exact_own_preset_and_foundation_without_optional_intent() {
        let (_a, a) = Fixture::new("2.0", Some(26));
        let (_b, b) = Fixture::new("1.0.2", None);
        let s = selected(&a);
        assert_eq!(resolve(&a, &s).unwrap(), HashSet::from([26]));
        assert!(resolve(&b, &s).unwrap().is_empty());
        let mut actual =
            ferrite_s101::viewing_groups_for_preset(&b, ferrite_s101::DisplayPreset::Standard)
                .unwrap();
        let original = actual.clone();
        actual.extend(resolve(&b, &s).unwrap());
        assert_eq!(actual, original);
        assert!(actual.contains(&1));
        assert!(actual.contains(&2));
    }
    #[test]
    fn explicitly_unknown_primary_and_unbound_ids_still_fail() {
        let (_b, b) = Fixture::new("1.0.2", None);
        let mut s = SettingsState::default();
        s.viewing_layers.insert("100".into());
        assert!(capture_primary(&b, &mut s)
            .unwrap_err()
            .to_string()
            .contains("Unknown S-101 viewing layer"));
        assert!(s.viewing_layer_owner.is_none());
        assert!(resolve(&b, &s).is_err());
    }
    #[test]
    fn identical_ids_with_different_owner_declarations_never_alias() {
        let (_a, a) = Fixture::new("2.0", Some(26));
        let (_b, b) = Fixture::new("other", Some(99));
        assert_eq!(resolve(&a, &selected(&a)).unwrap(), HashSet::from([26]));
        assert!(resolve(&b, &selected(&a)).unwrap().is_empty());
        assert_eq!(resolve(&b, &selected(&b)).unwrap(), HashSet::from([99]));
    }
    #[test]
    fn primary_replacement_is_candidate_only_and_does_not_inherit_by_version() {
        let (_a, a) = Fixture::new("2.0", Some(26));
        let (_b, b) = Fixture::new("2.0", Some(99));
        let old = selected(&a);
        let next = for_new_primary(&b, &old);
        assert!(next.viewing_layers.is_empty());
        assert!(next.viewing_layer_owner.is_none());
        assert_eq!(resolve(&a, &old).unwrap(), HashSet::from([26]));
        assert_eq!(old.viewing_layers, BTreeSet::from(["100".into()]));
    }
    #[test]
    fn same_owner_mutated_unknown_intent_is_revalidated_not_filtered() {
        let (_a, a) = Fixture::new("2.0", Some(26));
        let mut s = selected(&a);
        s.viewing_layers.insert("missing".into());
        assert!(resolve(&a, &s).is_err());
    }
    #[test]
    fn disabling_optional_does_not_remove_own_foundation() {
        let (_a, a) = Fixture::new("2.0", Some(26));
        let mut s = selected(&a);
        s.viewing_layers.clear();
        let mut groups =
            ferrite_s101::viewing_groups_for_preset(&a, ferrite_s101::DisplayPreset::Standard)
                .unwrap();
        groups.extend(resolve(&a, &s).unwrap());
        assert!(groups.contains(&1));
        assert!(groups.contains(&2));
        assert!(!groups.contains(&26));
    }
    #[test]
    fn cache_identity_covers_exact_owner_and_lexical_ids() {
        let hash = |s: &SettingsState| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            hash_identity(s, &mut h);
            h.finish()
        };
        let (_a, a) = Fixture::new("2.0", Some(26));
        let (_b, b) = Fixture::new("2.0", Some(99));
        let sa = selected(&a);
        let sb = selected(&b);
        assert_ne!(hash(&sa), hash(&sb));
        let mut changed = sa.clone();
        changed.viewing_layers.clear();
        assert_ne!(hash(&sa), hash(&changed));
        assert_eq!(hash(&sa), hash(&sa.clone()));
    }
}
