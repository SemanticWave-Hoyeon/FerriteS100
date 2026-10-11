//! Per-cell immutable PC/palette ownership. IR symbol names and source IDs are unchanged.
//! Additive preparation API: renderer routing/instance keys must adopt it before mixed-PC activation.
use crate::{Result, SymbolCache, WgpuError};
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, ColorProfile};
use ferrite_render::{ShallowPatternContract, SymbolId};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

/// Only an immutable bound PC can create an owner; token fields are not public.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortrayalResourceOwner {
    pc_digest: [u8; 32],
    palette: String,
    cache_revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OwnedSymbolKey {
    owner: PortrayalResourceOwner,
    symbol: SymbolId,
}
impl PortrayalResourceOwner {
    pub fn symbol_key(&self, symbol: SymbolId) -> OwnedSymbolKey {
        OwnedSymbolKey {
            owner: self.clone(),
            symbol,
        }
    }
    pub fn pc_digest(&self) -> &[u8; 32] {
        &self.pc_digest
    }
    pub fn palette(&self) -> &str {
        &self.palette
    }
    pub fn cache_revision(&self) -> u64 {
        self.cache_revision
    }
}
pub struct CellPortrayalResourceBinding {
    pub cell_index: usize,
    pub catalogue: Arc<BoundPortrayalCatalogue>,
    /// Validated product adapter contract, never inferred from a global name.
    pub shallow_pattern: Option<ShallowPatternContract>,
}
struct OwnerResources {
    catalogue: Arc<BoundPortrayalCatalogue>,
    profile: ColorProfile,
    cache: SymbolCache,
}
pub struct ResolvedPortrayalResources<'a> {
    pub owner: PortrayalResourceOwner,
    pub cache: &'a mut SymbolCache,
    pub profile: &'a ColorProfile,
}
/// A draw borrow is not a persistent symbol key or catalogue permission token.
/// The caller already has the registry; all validation is shared with resolve_mut.
pub(crate) struct DrawPortrayalResources<'a> {
    pub cache: &'a mut SymbolCache,
    pub profile: &'a ColorProfile,
}
pub struct CellPortrayalResources {
    /// Original cell index, not feature ordinal or product-edition string.
    cells: BTreeMap<usize, usize>,
    owners: Vec<OwnerResources>,
    cell_groups: BTreeMap<usize, Option<Arc<HashSet<u32>>>>,
    groups_sealed: bool,
    owner_group_plan: std::sync::Mutex<crate::owner_group_plan::Cache>,
    compact_admission: std::sync::Mutex<crate::compact_owner_admission::Cache>,
}
// Exact immutable inputs retained only for a synchronous invocation. Not a
// public permission token and never a persistent texture/cache namespace.
type TrialOwnerIdentity = ([u8; 32], String, u64, Option<ShallowPatternContract>);
pub(crate) struct TrialResourceIdentity {
    cells: BTreeMap<usize, usize>,
    groups: BTreeMap<usize, Option<Arc<HashSet<u32>>>>,
    owners: Vec<TrialOwnerIdentity>,
}
impl CellPortrayalResources {
    /// Entire private next registry, fresh mutable caches, no live owner mutation.
    /// Missing palette/duplicate cells/cap failure reject the complete preparation.
    pub fn prepare(
        bindings: &[CellPortrayalResourceBinding],
        palette: &str,
        max_cells: usize,
    ) -> Result<Self> {
        if bindings.len() > max_cells {
            return Err(WgpuError::Render(
                "PC owner binding host budget exceeded".into(),
            ));
        }
        let mut cells = BTreeMap::new();
        let mut owners: Vec<OwnerResources> = Vec::new();
        // Merge only identical immutable PC bytes and exact independent selector contracts.
        for binding in bindings {
            if cells.contains_key(&binding.cell_index) {
                return Err(WgpuError::Render("Duplicate cell PC owner".into()));
            }
            let shared = owners.iter().position(|row| {
                row.catalogue.source_digest() == binding.catalogue.source_digest()
                    && match (
                        row.cache.shallow_pattern_contract(),
                        binding.shallow_pattern.as_ref(),
                    ) {
                        (None, None) => true,
                        (Some(a), Some(b)) => a.same_bound_contract(b),
                        _ => false,
                    }
            });
            if let Some(index) = shared {
                cells.insert(binding.cell_index, index);
                continue;
            }
            let profile = binding
                .catalogue
                .color_profiles
                .get_profile(palette)
                .ok_or_else(|| {
                    WgpuError::Render(format!(
                        "PC {} lacks requested palette {palette}",
                        binding.catalogue.version
                    ))
                })?
                .clone();
            let cache = SymbolCache::new_with_pattern_contract(
                binding.catalogue.root_path.join("Symbols"),
                binding.catalogue.sources(),
                binding.shallow_pattern.clone(),
            );
            cells.insert(binding.cell_index, owners.len());
            owners.push(OwnerResources {
                catalogue: Arc::clone(&binding.catalogue),
                profile,
                cache,
            });
        }
        Ok(Self {
            cells,
            owners,
            cell_groups: Default::default(),
            groups_sealed: false,
            owner_group_plan: Default::default(),
            compact_admission: Default::default(),
        })
    }
    /// Capture the caller's validated preset/layer selection, independently
    /// for each source cell. None explicitly means unrestricted, not missing.
    /// Only an unpublished registry can change policy; failed validation leaves
    /// the previous captured selection untouched.
    pub fn capture_viewing_groups_for_cell(
        &mut self,
        cell: usize,
        groups: Option<HashSet<u32>>,
    ) -> Result<()> {
        if self.groups_sealed {
            return Err(WgpuError::Render(
                "Cell viewing-group policy already sealed".into(),
            ));
        }
        let index = *self.cells.get(&cell).ok_or_else(|| {
            WgpuError::Render(format!("No PC owner for viewing-group cell {cell}"))
        })?;
        if let Some(selected) = groups.as_ref() {
            if !selected.iter().all(|id| {
                self.owners[index]
                    .catalogue
                    .viewing_groups
                    .groups
                    .contains_key(id)
            }) {
                return Err(WgpuError::Render(format!(
                    "Viewing-group subset not declared by cell {cell} PC"
                )));
            }
        }
        self.cell_groups.insert(cell, groups.map(Arc::new));
        Ok(())
    }
    pub fn seal_viewing_groups(&mut self) -> Result<()> {
        if !self
            .cells
            .keys()
            .all(|cell| self.cell_groups.contains_key(cell))
        {
            return Err(WgpuError::Render(
                "Per-cell viewing-group policy has uncaptured owners".into(),
            ));
        }
        self.groups_sealed = true;
        Ok(())
    }
    pub fn viewing_groups_for_cell(&self, cell: Option<usize>) -> Result<Option<&HashSet<u32>>> {
        let cell = cell.ok_or_else(|| {
            WgpuError::Render("Viewing-group instruction lacks PC cell owner".into())
        })?;
        if !self.cells.contains_key(&cell) {
            return Err(WgpuError::Render(format!(
                "No viewing-group PC owner for cell {cell}"
            )));
        }
        let groups = self.cell_groups.get(&cell).ok_or_else(|| {
            WgpuError::Render(format!("No captured viewing-group policy for cell {cell}"))
        })?;
        Ok(groups.as_deref())
    }
    /// Only sealed immutable PC selections can prepare group-only decisions.
    pub(crate) fn prepare_owner_group_plan(
        &self,
        context: &ferrite_render::RenderContext,
    ) -> (Option<std::sync::Arc<crate::owner_group_plan::Plan>>, bool) {
        if !self.groups_sealed {
            return (None, false);
        }
        let Ok(mut cache) = self.owner_group_plan.lock() else {
            return (None, false);
        };
        cache.prepare(context, |instruction| {
            self.compiled_group_decision(instruction)
        })
    }
    // Shared original group-only compiler; called only after the sealed-policy guard.
    fn compiled_group_decision(
        &self,
        instruction: &ferrite_render::DrawingInstruction,
    ) -> Option<(bool, bool)> {
        // Match original ownership preconditions without constructing deferred error strings.
        let cell = instruction.cell_index()? as usize;
        if !self.cells.contains_key(&cell) {
            return None;
        }
        let groups = self.cell_groups.get(&cell)?.as_deref();
        let ordinary = groups.is_none_or(|selected| {
            instruction
                .viewing_groups()
                .all(|g| selected.contains(&g.0))
        });
        let overridden = groups.is_none_or(|selected| {
            instruction
                .viewing_groups()
                .all(|g| selected.contains(&g.0) || g.0 == 33010)
        });
        Some((ordinary, overridden))
    }
    /// No cached scene/view decision: this instance owns the sealed policy and private plan.
    /// None is whole original admission before any original owner callbacks ran.
    pub(crate) fn prepare_compact_admission(
        &self,
        context: &ferrite_render::RenderContext,
        execution: &[bool],
        scale: u32,
        override_group: Option<u32>,
        diagnostics: bool,
    ) -> Result<(
        Option<crate::compact_owner_admission::Frame>,
        crate::compact_owner_admission::Work,
    )> {
        let mut work = crate::compact_owner_admission::Work::default();
        if diagnostics {
            work.attempts = 1;
        }
        if !self.groups_sealed || !matches!(override_group, None | Some(33010)) {
            if diagnostics {
                work.declines = 1;
            }
            return Ok((None, work));
        }
        let Ok(mut cache) = self.compact_admission.lock() else {
            if diagnostics {
                work.declines = 1;
            }
            return Ok((None, work));
        };
        let start = diagnostics.then(std::time::Instant::now);
        let (plan, hit) = cache.prepare(context, |i| self.compiled_group_decision(i));
        if let Some(start) = start {
            work.prepare_host_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        }
        let Some(plan) = plan else {
            if diagnostics {
                work.declines = 1;
            }
            return Ok((None, work));
        };
        if diagnostics {
            work.hits = u64::from(hit);
            work.cold = u64::from(!hit);
            work.retained_payload_bytes = plan.charged_bytes() as u64;
        }
        let start = diagnostics.then(std::time::Instant::now);
        let result = plan.evaluate(context, execution, scale, override_group, |ordinal| {
            if diagnostics {
                work.original_owner_calls = work.original_owner_calls.saturating_add(1);
            }
            // Original function never takes this private cache lock, preserves original errors.
            self.instruction_visible_for_cell(
                &context.raw_instructions()[ordinal],
                scale,
                override_group,
            )
        });
        if let Some(start) = start {
            work.evaluate_host_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        }
        let frame = result?;
        if diagnostics {
            if let Some(frame) = &frame {
                work.descriptor_visits = execution.len() as u64;
                work.scheduled = frame.ordinals.len() as u64;
                work.frame_payload_bytes = frame.charged_bytes() as u64;
            } else {
                work.declines = 1;
            }
        }
        Ok((frame, work))
    }

    pub fn instruction_visible_for_cell(
        &self,
        instruction: &ferrite_render::DrawingInstruction,
        scale: u32,
        override_group: Option<u32>,
    ) -> Result<bool> {
        let groups = self.viewing_groups_for_cell(instruction.cell_index().map(|v| v as usize))?;
        Ok(ferrite_render::instruction_visible(
            instruction,
            scale,
            groups,
            override_group,
        ))
    }
    pub(crate) fn trial_identity(&self) -> Option<TrialResourceIdentity> {
        if !self.groups_sealed
            || !self
                .cells
                .keys()
                .all(|cell| self.cell_groups.contains_key(cell))
        {
            return None;
        }
        Some(TrialResourceIdentity {
            cells: self.cells.clone(),
            groups: self.cell_groups.clone(),
            owners: self
                .owners
                .iter()
                .map(|row| {
                    (
                        *row.catalogue.source_digest(),
                        row.profile.id.clone(),
                        row.cache.resource_revision(),
                        row.cache.shallow_pattern_contract().cloned(),
                    )
                })
                .collect(),
        })
    }
    pub(crate) fn matches_trial_identity(&self, previous: &TrialResourceIdentity) -> bool {
        self.groups_sealed
            && self.cells == previous.cells
            && self.cell_groups == previous.groups
            && self.owners.len() == previous.owners.len()
            && self.owners.iter().zip(&previous.owners).all(
                |(row, (digest, palette, revision, contract))| {
                    row.catalogue.source_digest() == digest
                        && &row.profile.id == palette
                        && row.cache.resource_revision() == *revision
                        && match (row.cache.shallow_pattern_contract(), contract.as_ref()) {
                            (None, None) => true,
                            (Some(a), Some(b)) => a.same_bound_contract(b),
                            _ => false,
                        }
                },
            )
    }
    pub fn resolve_mut(
        &mut self,
        cell_index: Option<usize>,
    ) -> Result<ResolvedPortrayalResources<'_>> {
        let row = self.resolve_row_mut(cell_index)?;
        let owner = PortrayalResourceOwner {
            pc_digest: *row.catalogue.source_digest(),
            palette: row.profile.id.clone(),
            cache_revision: row.cache.resource_revision(),
        };
        Ok(ResolvedPortrayalResources {
            owner,
            cache: &mut row.cache,
            profile: &row.profile,
        })
    }
    // Shared original checks/order and borrowed row identity. No cached pointer/index
    // escapes the immutable registry and no policy/global-PC fallback is introduced.
    fn resolve_row_mut(&mut self, cell_index: Option<usize>) -> Result<&mut OwnerResources> {
        let cell = cell_index
            .ok_or_else(|| WgpuError::Render("ENC resource lacks cell PC owner".into()))?;
        let index = *self.cells.get(&cell).ok_or_else(|| {
            WgpuError::Render(format!("No immutable PC resource owner for cell {cell}"))
        })?;
        Ok(&mut self.owners[index])
    }
    pub(crate) fn resolve_draw_mut(
        &mut self,
        cell_index: Option<usize>,
    ) -> Result<DrawPortrayalResources<'_>> {
        let row = self.resolve_row_mut(cell_index)?;
        Ok(DrawPortrayalResources {
            cache: &mut row.cache,
            profile: &row.profile,
        })
    }

    pub fn cell_profiles(&self) -> impl Iterator<Item = (usize, &ColorProfile)> {
        self.cells
            .iter()
            .map(|(cell, index)| (*cell, &self.owners[*index].profile))
    }
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn owner(digest: u8, palette: &str, revision: u64) -> PortrayalResourceOwner {
        PortrayalResourceOwner {
            pc_digest: [digest; 32],
            palette: palette.into(),
            cache_revision: revision,
        }
    }
    #[test]
    fn identical_symbol_names_cannot_alias_pc_palette_or_cache_revision() {
        let symbol = ferrite_render::intern_symbol("SYM_SOUNDING");
        let base = owner(1, "Day", 1).symbol_key(symbol);
        for different in [owner(2, "Day", 1), owner(1, "Night", 1), owner(1, "Day", 2)] {
            assert_ne!(base, different.symbol_key(symbol));
        }
        assert_eq!(base, owner(1, "Day", 1).symbol_key(symbol));
        assert_eq!(ferrite_render::resolve_symbol(symbol), "SYM_SOUNDING");
    }
    #[test]
    fn empty_registry_never_silently_routes_missing_cells_to_global_pc() {
        let mut registry = CellPortrayalResources::prepare(&[], "Day", 0).unwrap();
        assert_eq!(registry.cell_count(), 0);
        assert!(registry.resolve_mut(None).is_err());
        assert!(registry.resolve_mut(Some(0)).is_err());
    }
}

#[cfg(test)]
mod group_policy_tests {
    use super::*;
    fn policy_fixture() -> CellPortrayalResources {
        CellPortrayalResources {
            cells: BTreeMap::from([(0, 0), (1, 0)]),
            owners: Vec::new(),
            cell_groups: BTreeMap::new(),
            groups_sealed: false,
            owner_group_plan: Default::default(),
            compact_admission: Default::default(),
        }
    }
    #[test]
    fn missing_capture_differs_from_explicit_unrestricted_and_empty() {
        let mut r = policy_fixture();
        assert!(r.seal_viewing_groups().is_err());
        assert!(r.viewing_groups_for_cell(Some(0)).is_err());
        r.cell_groups.insert(0, None);
        r.cell_groups.insert(1, Some(Arc::new(HashSet::new())));
        r.seal_viewing_groups().unwrap();
        assert!(r.viewing_groups_for_cell(Some(0)).unwrap().is_none());
        assert!(r
            .viewing_groups_for_cell(Some(1))
            .unwrap()
            .unwrap()
            .is_empty());
        assert!(r.capture_viewing_groups_for_cell(0, None).is_err());
        assert!(r.viewing_groups_for_cell(Some(2)).is_err());
    }
    #[test]
    fn shared_pc_cache_does_not_merge_independent_cell_group_visibility() {
        let mut r = policy_fixture();
        r.cell_groups
            .insert(0, Some(Arc::new(HashSet::from([90000]))));
        r.cell_groups
            .insert(1, Some(Arc::new(HashSet::from([13030]))));
        r.seal_viewing_groups().unwrap();
        let groups0 = r.viewing_groups_for_cell(Some(0)).unwrap().unwrap();
        let groups1 = r.viewing_groups_for_cell(Some(1)).unwrap().unwrap();
        assert!(groups0.contains(&90000) && !groups0.contains(&13030));
        assert!(!groups1.contains(&90000) && groups1.contains(&13030));
    }
}

#[cfg(test)]
mod group_execution_tests {
    use super::*;
    use ferrite_render::{DrawingInstruction, LineInstruction, PointInstruction, WorldPoint};
    fn fixture() -> CellPortrayalResources {
        CellPortrayalResources {
            cells: BTreeMap::from([(0, 0), (1, 0)]),
            owners: Vec::new(),
            cell_groups: BTreeMap::from([
                (0, Some(Arc::new(HashSet::from([90000])))),
                (1, Some(Arc::new(HashSet::from([13030])))),
            ]),
            groups_sealed: true,
            owner_group_plan: Default::default(),
            compact_admission: Default::default(),
        }
    }
    #[test]
    fn owner_group_filter_never_uses_other_pc_union_and_preserves_all_required_groups() {
        let r = fixture();
        let point = |cell, groups: &[u32]| {
            DrawingInstruction::Point(
                PointInstruction::new("SAME_NAME".into(), WorldPoint::new(0., 0.))
                    .with_cell_index(cell)
                    .with_viewing_groups(groups),
            )
        };
        assert!(!r
            .instruction_visible_for_cell(&point(0, &[13030]), 10000, None)
            .unwrap());
        assert!(r
            .instruction_visible_for_cell(&point(1, &[13030]), 10000, None)
            .unwrap());
        assert!(!r
            .instruction_visible_for_cell(&point(0, &[90000, 13030]), 10000, None)
            .unwrap());
        assert!(r
            .instruction_visible_for_cell(&point(0, &[90000, 33010]), 10000, Some(33010))
            .unwrap());
        assert!(!r
            .instruction_visible_for_cell(&point(0, &[90000, 33010]), 10000, None)
            .unwrap());
    }
    #[test]
    fn hidden_high_priority_foreign_group_line_cannot_suppress_visible_owner_line() {
        let r = fixture();
        let line = |cell, priority| {
            DrawingInstruction::Line(
                LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(10., 0.)])
                    .with_cell_index(cell)
                    .with_priority(priority)
                    .with_viewing_groups(&[13030]),
            )
        };
        let lines = [line(1, 2), line(0, 9)];
        let mask: Vec<_> = lines
            .iter()
            .map(|line| r.instruction_visible_for_cell(line, 10000, None).unwrap())
            .collect();
        assert_eq!(mask, [true, false]);
        let mut cache = ferrite_render::LineSuppressionCache::default();
        let plan = cache.plan_with_visibility(&lines, 10000, None, None, Some(&mask));
        assert!(!plan.contains(&0));
        let legacy_union = HashSet::from([90000, 13030]);
        let wrong = cache.plan_with_visibility(&lines, 10000, Some(&legacy_union), None, None);
        assert!(wrong.contains(&0));
    }
}

#[cfg(test)]
mod trial_identity_tests {
    use super::*;
    fn fixture() -> CellPortrayalResources {
        CellPortrayalResources {
            cells: BTreeMap::from([(0, 0), (1, 0)]),
            owners: Vec::new(),
            cell_groups: BTreeMap::from([(0, Some(Arc::new(HashSet::from([90000])))), (1, None)]),
            groups_sealed: true,
            owner_group_plan: Default::default(),
            compact_admission: Default::default(),
        }
    }
    #[test]
    fn sealed_cell_mapping_and_group_contents_bound_exactly() {
        let mut r = fixture();
        let before = r.trial_identity().unwrap();
        assert!(r.matches_trial_identity(&before));
        r.cell_groups
            .insert(0, Some(Arc::new(HashSet::from([13030]))));
        assert!(!r.matches_trial_identity(&before));
        let current = r.trial_identity().unwrap();
        r.cells.insert(1, 1);
        assert!(!r.matches_trial_identity(&current));
    }
    #[test]
    fn unsealed_or_incomplete_registry_never_captures_readiness() {
        let mut r = fixture();
        r.groups_sealed = false;
        assert!(r.trial_identity().is_none());
        r.groups_sealed = true;
        r.cell_groups.remove(&1);
        assert!(r.trial_identity().is_none());
    }
}

#[cfg(test)]
mod owner_plan_controls {
    use super::*;
    use ferrite_render::{
        DrawingInstruction, PointInstruction, RenderContext, Viewport, WorldPoint,
    };
    fn fixture() -> CellPortrayalResources {
        CellPortrayalResources {
            cells: BTreeMap::from([(0, 0), (1, 1)]),
            owners: vec![],
            cell_groups: BTreeMap::from([
                (0, Some(Arc::new(HashSet::from([90000])))),
                (1, Some(Arc::new(HashSet::from([13030])))),
            ]),
            groups_sealed: true,
            owner_group_plan: Default::default(),
            compact_admission: Default::default(),
        }
    }
    fn point(cell: usize, groups: &[u32]) -> DrawingInstruction {
        DrawingInstruction::Point(
            PointInstruction::new("A".into(), WorldPoint::new(0., 0.))
                .with_cell_index(cell)
                .with_viewing_groups(groups),
        )
    }
    fn apply(
        resources: &CellPortrayalResources,
        c: &RenderContext,
        execution: &[bool],
        optimized: bool,
        override_group: Option<u32>,
    ) -> Result<Vec<bool>> {
        let plan = if optimized {
            resources.prepare_owner_group_plan(c).0
        } else {
            None
        };
        c.raw_instructions()
            .iter()
            .enumerate()
            .map(|(i, instruction)| {
                if !execution[i] {
                    return Ok(false);
                }
                match plan
                    .as_ref()
                    .and_then(|p| p.group_visible(i, override_group))
                {
                    Some(group) => {
                        Ok(
                            ferrite_render::instruction_visible(instruction, 10000, None, None)
                                && group,
                        )
                    }
                    None => {
                        resources.instruction_visible_for_cell(instruction, 10000, override_group)
                    }
                }
            })
            .collect()
    }
    #[test]
    fn actual_resource_sets_and_all_groups_match_original() {
        let r = fixture();
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        for p in [
            point(0, &[90000]),
            point(1, &[90000]),
            point(1, &[13030]),
            point(0, &[90000, 13030]),
            point(0, &[33010]),
        ] {
            c.add_instruction(p)
        }
        for override_group in [None, Some(33010), Some(999)] {
            assert_eq!(
                apply(&r, &c, &[true; 5], true, override_group).unwrap(),
                apply(&r, &c, &[true; 5], false, override_group).unwrap()
            )
        }
    }
    #[test]
    fn hidden_bad_owner_is_deferred_then_original_error_order() {
        let r = fixture();
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(point(0, &[90000]));
        c.add_instruction(point(9, &[90000]));
        c.add_instruction(point(8, &[90000]));
        assert_eq!(
            apply(&r, &c, &[true, false, false], true, None).unwrap(),
            vec![true, false, false]
        );
        for execution in [[true, true, true], [true, false, true]] {
            let a = apply(&r, &c, &execution, true, None)
                .unwrap_err()
                .to_string();
            let b = apply(&r, &c, &execution, false, None)
                .unwrap_err()
                .to_string();
            assert_eq!(a, b)
        }
    }
    #[test]
    fn unsealed_policy_and_new_owner_fallback() {
        let mut r = fixture();
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(point(0, &[90000]));
        assert!(r.prepare_owner_group_plan(&c).0.is_some());
        r.groups_sealed = false;
        assert!(r.prepare_owner_group_plan(&c).0.is_none());
        let mut other = fixture();
        other.cell_groups.insert(0, Some(Arc::new(HashSet::new())));
        assert_eq!(apply(&other, &c, &[true], true, None).unwrap(), vec![false]);
        assert_eq!(apply(&r, &c, &[true], true, None).unwrap(), vec![true]);
    }
}

#[cfg(test)]
mod draw_borrow_controls {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "ferrite-draw-borrow-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&p).unwrap();
            std::fs::create_dir(p.join("Symbols")).unwrap();
            std::fs::write(p.join("portrayal_catalogue.xml"),
                "<portrayalCatalog productId='S-101' version='A'><foundationMode/><displayPlanes><displayPlane id='OverRadar' order='1'/></displayPlanes></portrayalCatalog>").unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fixture(t: &Temp) -> CellPortrayalResources {
        let catalogue =
            Arc::new(ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&t.0).unwrap());
        CellPortrayalResources {
            cells: BTreeMap::from([(0, 0), (1, 0), (2, 1)]),
            owners: ["Day", "Night"]
                .iter()
                .map(|name| OwnerResources {
                    catalogue: catalogue.clone(),
                    profile: ColorProfile::new((*name).into(), (*name).into()),
                    cache: SymbolCache::new_with_sources(t.0.join("Symbols"), catalogue.sources()),
                })
                .collect(),
            cell_groups: BTreeMap::from([(0, None), (1, None), (2, None)]),
            groups_sealed: true,
            owner_group_plan: Default::default(),
            compact_admission: Default::default(),
        }
    }
    #[test]
    fn draw_and_original_share_exact_cache_profile_for_shared_and_distinct_cells() {
        let t = Temp::new();
        let mut r = fixture(&t);
        for cell in [0, 1, 2, 0] {
            let old = r.resolve_mut(Some(cell)).unwrap();
            let old_cache = old.cache as *mut SymbolCache;
            let old_profile = old.profile as *const ColorProfile;
            let old_palette = old.owner.palette().to_owned();
            let new = r.resolve_draw_mut(Some(cell)).unwrap();
            assert_eq!(old_cache, new.cache as *mut SymbolCache);
            assert_eq!(old_profile, new.profile as *const ColorProfile);
            assert_eq!(old_palette, new.profile.id);
        }
        assert_eq!(r.resolve_draw_mut(Some(2)).unwrap().profile.id, "Night");
    }
    #[test]
    fn missing_cell_errors_match_original_and_never_use_global_owner() {
        let t = Temp::new();
        let mut r = fixture(&t);
        for cell in [None, Some(99)] {
            let old = r.resolve_mut(cell).err().unwrap().to_string();
            let new = r.resolve_draw_mut(cell).err().unwrap().to_string();
            assert_eq!(old, new);
        }
    }
    #[test]
    fn cache_mutation_revision_and_registry_replacement_remain_live() {
        let t = Temp::new();
        let mut a = fixture(&t);
        let mut b = fixture(&t);
        let revision = a.resolve_mut(Some(0)).unwrap().owner.cache_revision;
        let b_revision = b.resolve_mut(Some(0)).unwrap().owner.cache_revision;
        a.resolve_draw_mut(Some(1)).unwrap().cache.clear();
        assert!(a.resolve_mut(Some(0)).unwrap().owner.cache_revision > revision);
        assert_eq!(
            b.resolve_mut(Some(0)).unwrap().owner.cache_revision,
            b_revision
        );
        let old = a.resolve_draw_mut(Some(0)).unwrap().cache as *mut SymbolCache;
        assert_ne!(
            old,
            b.resolve_draw_mut(Some(0)).unwrap().cache as *mut SymbolCache
        );
    }
}

#[cfg(test)]
mod compact_admission_registry_controls {
    use super::*;
    use ferrite_render::{
        DrawingInstruction, PointInstruction, RenderContext, ScaleRange, Viewport, WorldPoint,
    };
    fn registry(a: &[u32], b: &[u32]) -> CellPortrayalResources {
        CellPortrayalResources {
            cells: BTreeMap::from([(0, 0), (1, 0)]),
            owners: Vec::new(),
            cell_groups: BTreeMap::from([
                (0, Some(Arc::new(a.iter().copied().collect()))),
                (1, Some(Arc::new(b.iter().copied().collect()))),
            ]),
            groups_sealed: true,
            owner_group_plan: Default::default(),
            compact_admission: Default::default(),
        }
    }
    fn point(cell: usize, groups: &[u32]) -> DrawingInstruction {
        DrawingInstruction::Point(
            PointInstruction::new("X".into(), WorldPoint::new(0., 0.))
                .with_cell_index(cell)
                .with_viewing_groups(groups),
        )
    }
    fn legacy(
        r: &CellPortrayalResources,
        c: &RenderContext,
        e: &[bool],
        scale: u32,
        over: Option<u32>,
    ) -> Result<Vec<bool>> {
        c.raw_instructions()
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if e[i] {
                    r.instruction_visible_for_cell(p, scale, over)
                } else {
                    Ok(false)
                }
            })
            .collect()
    }
    // Draft oracle: independently exercises the CURRENT registry and compact
    // evaluator. It is not an end-to-end emitter/material/coverage proof.
    #[test]
    fn current_admission_implies_original_dispatch_and_scale_for_all_truth_inputs() {
        use ferrite_render::{LineInstruction, ScaleRange};
        let r = registry(&[1], &[2]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        for cell in [0, 1] {
            for groups in [&[1][..], &[2][..], &[1, 2][..], &[33010][..]] {
                for (lower, upper) in [
                    (None, None),
                    (Some(0), Some(0)),
                    (Some(10), Some(20)),
                    (Some(20), Some(10)),
                ] {
                    let mut p = PointInstruction::new("X".into(), WorldPoint::new(0., 0.))
                        .with_cell_index(cell)
                        .with_viewing_groups(groups);
                    p.scale_range = ScaleRange {
                        scale_maximum: lower,
                        scale_minimum: upper,
                    };
                    c.add_instruction(DrawingInstruction::Point(p));
                }
            }
        }
        // Original visible-stroke predicate, including zero and alpha-null lines.
        for (width, alpha) in [(0., 1.), (1., 0.), (1., 1.)] {
            let mut l =
                LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)]);
            l.cell_index = Some(0);
            l.viewing_group = ferrite_render::ViewingGroup(1);
            l.style.width = width;
            l.style.color.a = alpha;
            c.add_instruction(DrawingInstruction::Line(l));
        }
        c.get_sorted_instructions();
        let n = c.instruction_count();
        for scale in [0, 1, 9, 10, 20, 21, u32::MAX] {
            for over in [None, Some(33010)] {
                for phase in 0..3 {
                    let execution: Vec<_> = (0..n).map(|i| (i + phase) % 3 != 0).collect();
                    let original = legacy(&r, &c, &execution, scale, over).unwrap();
                    let (compact, _) = r
                        .prepare_compact_admission(&c, &execution, scale, over, false)
                        .unwrap();
                    let compact = compact.unwrap();
                    assert_eq!(compact.mask, original);
                    for (i, p) in c.raw_instructions().iter().enumerate() {
                        if original[i] {
                            assert!(ferrite_render::instruction_visible(p, scale, None, over));
                            assert!(p.scale_range().is_visible_at(scale));
                        }
                    }
                    // Baseline's second pure predicate produces the same ordinal
                    // sequence as the already admitted mask; geometry is untouched.
                    let baseline: Vec<_> = (0..n)
                        .filter(|&i| {
                            original[i]
                                && ferrite_render::instruction_visible(
                                    &c.raw_instructions()[i],
                                    scale,
                                    None,
                                    over,
                                )
                                && c.raw_instructions()[i].scale_range().is_visible_at(scale)
                        })
                        .collect();
                    let candidate: Vec<_> = (0..n).filter(|&i| original[i]).collect();
                    assert_eq!(baseline, candidate);
                }
            }
        }
    }
    #[test]
    fn current_admission_does_not_mask_unknown_owner_error_or_promote_nonexecuted_source() {
        let r = registry(&[1], &[2]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(point(0, &[1]));
        c.add_instruction(point(99, &[1]));
        c.get_sorted_instructions();
        let bad = c
            .raw_instructions()
            .iter()
            .position(|p| p.cell_index() == Some(99))
            .unwrap();
        let mut execution = vec![true; c.instruction_count()];
        execution[bad] = false;
        assert!(legacy(&r, &c, &execution, 1, None).is_ok());
        assert!(r
            .prepare_compact_admission(&c, &execution, 1, None, false)
            .is_ok());
        execution[bad] = true;
        let expected = legacy(&r, &c, &execution, 1, None).unwrap_err().to_string();
        let observed = r
            .prepare_compact_admission(&c, &execution, 1, None, false)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(observed, expected);
    }
    #[test]
    fn full_registry_model_all_masks_scale_override_temporal_input_unchanged() {
        let r = registry(&[1], &[2]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        for p in [
            point(0, &[1]),
            point(0, &[1, 2]),
            point(1, &[2]),
            point(1, &[33010]),
        ] {
            c.add_instruction(p);
        }
        for bits in 0..16 {
            for scale in [0, 1, 10000, u32::MAX] {
                for over in [None, Some(33010)] {
                    let temporal: Vec<bool> = (0..4).map(|i| bits & (1 << i) != 0).collect();
                    let original_temporal = temporal.clone();
                    let (f, _) = r
                        .prepare_compact_admission(&c, &temporal, scale, over, true)
                        .unwrap();
                    let f = f.unwrap();
                    let expected = legacy(&r, &c, &temporal, scale, over).unwrap();
                    assert_eq!(f.mask, expected);
                    assert_eq!(temporal, original_temporal);
                    assert_eq!(
                        crate::compact_owner_admission::Ordinals::new(4, Some(&f))
                            .collect::<Vec<_>>(),
                        expected
                            .iter()
                            .enumerate()
                            .filter_map(|(i, v)| v.then_some(i))
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
    }
    #[test]
    fn owner_replacement_unsealed_and_poison_degrade_to_full_original() {
        let a = registry(&[1], &[2]);
        let b = registry(&[2], &[1]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(point(0, &[1]));
        let (f, w) = a
            .prepare_compact_admission(&c, &[true], 1, None, true)
            .unwrap();
        assert_eq!(f.unwrap().mask, vec![true]);
        assert_eq!(w.cold, 1);
        assert_eq!(
            a.prepare_compact_admission(&c, &[true], 2, None, true)
                .unwrap()
                .1
                .hits,
            1
        );
        assert_eq!(
            b.prepare_compact_admission(&c, &[true], 1, None, true)
                .unwrap()
                .0
                .unwrap()
                .mask,
            vec![false]
        );
        let mut unsealed = registry(&[1], &[2]);
        unsealed.groups_sealed = false;
        assert!(unsealed
            .prepare_compact_admission(&c, &[true], 1, None, true)
            .unwrap()
            .0
            .is_none());
        let poisoned = registry(&[1], &[2]);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.compact_admission.lock().unwrap();
            panic!("inject mutex poison");
        }));
        assert!(poisoned
            .prepare_compact_admission(&c, &[true], 1, None, true)
            .unwrap()
            .0
            .is_none());
        assert_eq!(legacy(&poisoned, &c, &[true], 1, None).unwrap(), vec![true]);
    }
    #[test]
    fn append_reorder_new_context_is_cold_and_keeps_original_ordinals() {
        let r = registry(&[1], &[2]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(point(0, &[1]));
        assert_eq!(
            r.prepare_compact_admission(&c, &[true], 1, None, true)
                .unwrap()
                .1
                .cold,
            1
        );
        c.add_instruction(point(1, &[1]));
        let (f, w) = r
            .prepare_compact_admission(&c, &[true, true], 1, None, true)
            .unwrap();
        assert_eq!(w.cold, 1);
        assert_eq!(f.unwrap().ordinals, vec![0]);
        let mut reordered = RenderContext::new(Viewport::new(10., 10.));
        reordered.add_instruction(point(1, &[1]));
        reordered.add_instruction(point(0, &[1]));
        let (f, w) = r
            .prepare_compact_admission(&reordered, &[true, true], 1, None, true)
            .unwrap();
        assert_eq!(w.cold, 1);
        assert_eq!(f.unwrap().ordinals, vec![1]);
    }
    #[test]
    fn hidden_error_and_scale_hidden_error_match_first_original_owner_error() {
        let r = registry(&[1], &[2]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(point(0, &[1]));
        let mut bad = PointInstruction::new("X".into(), WorldPoint::new(0., 0.)).with_cell_index(9);
        bad.scale_range = ScaleRange {
            scale_minimum: Some(1),
            scale_maximum: None,
        };
        c.add_instruction(DrawingInstruction::Point(bad));
        c.add_instruction(point(8, &[1]));
        let (frame, _) = r
            .prepare_compact_admission(&c, &[true, false, false], 10000, None, true)
            .unwrap();
        assert_eq!(frame.unwrap().mask, vec![true, false, false]);
        for e in [[true, true, true], [true, false, true]] {
            let old = legacy(&r, &c, &e, 10000, None).err().unwrap().to_string();
            let new = r
                .prepare_compact_admission(&c, &e, 10000, None, true)
                .err()
                .unwrap()
                .to_string();
            assert_eq!(old, new);
        }
        assert!(r
            .prepare_compact_admission(&c, &[true, false, false], 1, Some(999), true)
            .unwrap()
            .0
            .is_none());
    }
    #[test]
    fn all_instruction_kinds_keep_geometry_owner_order_and_exact_scale_boundaries() {
        use ferrite_render::{AreaInstruction, LineInstruction, TextInstruction};
        let r = registry(&[1], &[2]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        let range = ScaleRange {
            scale_maximum: Some(10),
            scale_minimum: Some(20),
        };
        c.add_instruction(point(0, &[1]));
        c.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(-0., 1.), WorldPoint::new(3., 4.)])
                .with_cell_index(1)
                .with_viewing_groups(&[2])
                .with_scale_range(range),
        ));
        c.add_instruction(DrawingInstruction::Area(
            AreaInstruction::new(vec![
                WorldPoint::new(0., 0.),
                WorldPoint::new(1., 0.),
                WorldPoint::new(1., 1.),
            ])
            .with_cell_index(0)
            .with_viewing_groups(&[1])
            .with_scale_range(range),
        ));
        c.add_instruction(DrawingInstruction::Text(
            TextInstruction::new("mandatory".into(), WorldPoint::new(5., 6.))
                .with_cell_index(1)
                .with_viewing_groups(&[2])
                .with_scale_range(range),
        ));
        // Serialized full IR, including geometry bits/owner fields, must not be mutated by admission.
        let original = serde_json::to_vec(c.raw_instructions()).unwrap();
        for scale in [0, 9, 10, 20, 21, u32::MAX] {
            for bits in 0..16 {
                let e: Vec<bool> = (0..4).map(|i| bits & (1 << i) != 0).collect();
                let (f, _) = r
                    .prepare_compact_admission(&c, &e, scale, None, true)
                    .unwrap();
                let f = f.unwrap();
                assert_eq!(f.mask, legacy(&r, &c, &e, scale, None).unwrap());
                assert_eq!(serde_json::to_vec(c.raw_instructions()).unwrap(), original);
            }
        }
    }
    #[test]
    fn failed_candidate_retains_old_frame_and_owner_cache_can_return_to_old_source() {
        let r = registry(&[1], &[2]);
        let mut old = RenderContext::new(Viewport::new(10., 10.));
        old.add_instruction(point(0, &[1]));
        let (old_frame, _) = r
            .prepare_compact_admission(&old, &[true], 1, None, true)
            .unwrap();
        let old_frame = old_frame.unwrap();
        let mut next = RenderContext::new(Viewport::new(10., 10.));
        next.add_instruction(point(0, &[1]));
        next.add_instruction(point(9, &[1]));
        assert!(r
            .prepare_compact_admission(&next, &[true, true], 1, None, true)
            .is_err());
        assert_eq!(old_frame.mask, vec![true]);
        assert_eq!(old_frame.ordinals, vec![0]);
        let (restored, work) = r
            .prepare_compact_admission(&old, &[true], 1, None, true)
            .unwrap();
        assert_eq!(work.cold, 1);
        assert_eq!(restored.unwrap().mask, old_frame.mask);
        // This proves local admission transaction only; App/GPU rollback remains a native gate.
    }
    #[test]
    fn simultaneous_existing_group_compact_and_frame_actual_capacities_are_bounded() {
        let r = registry(&[1], &[2]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        for cell in [0, 1, 0, 1] {
            c.add_instruction(point(cell, &[1]));
        }
        let (old, _) = r.prepare_owner_group_plan(&c);
        let old = old.unwrap();
        let (frame, work) = r
            .prepare_compact_admission(&c, &[true; 4], 1, None, true)
            .unwrap();
        let frame = frame.unwrap();
        let old_charge = old.charged_bytes();
        assert!(old_charge <= 1024 * 1024);
        assert!(
            work.retained_payload_bytes as usize <= crate::compact_owner_admission::RETAINED_CAP
        );
        assert!(frame.charged_bytes() <= crate::compact_owner_admission::FRAME_CAP);
        assert_eq!(frame.charged_bytes() as u64, work.frame_payload_bytes);
        let compact_guard = r.compact_admission.lock().unwrap();
        // Cache returns only a borrow; no independently retainable compact Plan/Arc escapes.
        drop(compact_guard);
        let sum = old_charge
            .checked_add(work.retained_payload_bytes as usize)
            .unwrap()
            .checked_add(frame.charged_bytes())
            .unwrap();
        assert!(sum <= 5 * 1024 * 1024);
        assert_eq!(frame.mask, legacy(&r, &c, &[true; 4], 1, None).unwrap());
    }
    #[test]
    fn same_context_same_registry_palette_alpha_recompiles_and_matches_original() {
        use ferrite_render::{Color, LineInstruction};
        let r = registry(&[1], &[2]);
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        let mut line =
            LineInstruction::new(vec![WorldPoint::new(-0., 1.), WorldPoint::new(2., 3.)])
                .with_cell_index(0)
                .with_viewing_groups(&[1]);
        line.color_token = Some("LINE".into());
        line.style.opacity = 0.5;
        c.add_instruction(DrawingInstruction::Line(line));
        c.get_sorted_instructions();
        assert_eq!(
            r.prepare_compact_admission(&c, &[true], 1, None, true)
                .unwrap()
                .1
                .cold,
            1
        );
        for alpha in [1., 0., f32::NAN, f32::INFINITY, -1., 1.] {
            c.remap_colors(&|_| Color::rgba(0., 0., 0., alpha));
            let (frame, work) = r
                .prepare_compact_admission(&c, &[true], 1, None, true)
                .unwrap();
            let frame = frame.unwrap();
            assert_eq!(work.cold, 1);
            assert_eq!(work.hits, 0);
            let expected = legacy(&r, &c, &[true], 1, None).unwrap();
            assert_eq!(frame.mask, expected);
            assert_eq!(frame.ordinals, if expected[0] { vec![0] } else { vec![] });
            let (warm, work) = r
                .prepare_compact_admission(&c, &[true], 1, None, true)
                .unwrap();
            assert_eq!(work.hits, 1);
            assert_eq!(warm.unwrap().mask, expected);
        }
    }
}
