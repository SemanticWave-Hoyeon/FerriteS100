//! The S-124 overlay module: the navigational warning lifecycle over the
//! loaded S-124 datasets (status, cancellation, in-force bulletins and fixed
//! date ranges at the viewing time), the user's classifications and
//! indications with their per-user persistence (11.1.1), the portrayal
//! selection (11.2) and the warning-list details of picked features.
use crate::navwarn::{
    self, CancelReason, NavwarnInput, NavwarnRegistry, NavwarnUserState, PartTiming,
    PortrayalPolicy, SortOrder, Status, UserClassification,
};
use crate::rules::{IMPORT_EXTENSIONS, S124};
use crate::user_state::{encode_user_state, read_user_record, set_aside_record, StoredUserState};
use chrono::{DateTime, Utc};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_product_api::platform::UserRecord;
use ferrite_product_api::{
    Encoding, PanelDescriptor, ProductAdapter, ProductInfo, Recognised, SceneRole,
};
use ferrite_s100_gml::overlay::{format_time, OverlayCore};
use ferrite_s100_gml::portrayal::ConvertOptions;
use ferrite_s100_gml::product::{GmlOverlayProduct, GmlRules, PortrayalSettings};
use ferrite_s100_gml::Dataset;
use std::any::Any;
use std::collections::HashSet;
use std::path::Path;
use tracing::{info, warn};

/// Viewing groups used by the S-124 2.0.0 XSLT rules but not declared in its
/// portrayal_catalogue.xml (TextPlacement text and co-located part counts).
pub const IMPLICIT_GROUPS: [u32; 2] = [26, 12210];

/// Where the user state is persisted for the current OS user.
pub struct UserStateStore {
    pub record: Box<dyn UserRecord>,
    pub user: String,
    pub status: String,
}

/// The S-124 overlay module.
pub struct S124Overlay {
    /// The user's classifications, displayed marks and first receipts.
    pub user: NavwarnUserState,
    pub policy: PortrayalPolicy,
    /// Warning list order.
    pub sort: SortOrder,
    registry: Option<NavwarnRegistry>,
    /// Dataset ids in registry input order.
    pub registry_ids: Vec<u32>,
    evaluated: Option<ferrite_kernel::time::Instant>,
    /// New-message indications of the last evaluation (registry order).
    new_flags: Vec<bool>,
    user_store: Option<UserStateStore>,
}

impl Default for S124Overlay {
    fn default() -> Self {
        Self {
            user: NavwarnUserState::default(),
            policy: PortrayalPolicy::default(),
            sort: SortOrder::Received,
            registry: None,
            registry_ids: Vec::new(),
            evaluated: None,
            new_flags: Vec::new(),
            user_store: None,
        }
    }
}

impl S124Overlay {
    fn registry_index(&self, id: u32) -> Option<usize> {
        self.registry_ids.iter().position(|d| *d == id)
    }

    /// The lifecycle evaluation (evaluated now when invalidated).
    pub fn registry(&mut self, core: &mut OverlayCore) -> &NavwarnRegistry {
        if self.registry.is_none() {
            self.evaluate(core, true);
        }
        self.registry.as_ref().expect("evaluated")
    }

    /// Classify a warning (11.1.2); persisted with the user state.
    pub fn classify(&mut self, core: &mut OverlayCore, id: u32, class: UserClassification) {
        self.registry(core);
        let Some(index) = self.registry_index(id) else {
            return;
        };
        let registry = self.registry.as_ref().expect("evaluated");
        if let Some(m) = registry.message(index) {
            if self.user.classification_of(m) != class {
                self.user.classify(m, class);
                self.persist_user_state(core);
            }
            core.bump_revision();
            self.registry = None;
        }
    }

    /// Mark a warning as displayed (ends its new-message indication).
    pub fn mark_displayed(&mut self, core: &mut OverlayCore, id: u32) {
        self.registry(core);
        if let Some(m) = self
            .registry_index(id)
            .and_then(|i| self.registry.as_ref().expect("evaluated").message(i))
        {
            if !self.user.displayed.contains(&m.dataset_key) {
                let m = m.clone();
                self.user.mark_displayed(&m);
                self.persist_user_state(core);
                core.ui_revision = 0;
            }
        }
    }

    /// Status line of the per-user state, for the warning list.
    pub fn user_status(&self) -> String {
        self.user_store
            .as_ref()
            .map(|s| s.status.clone())
            .unwrap_or_else(|| "User state not saved".into())
    }

    fn persist_user_state(&mut self, core: &mut OverlayCore) {
        let now = core.now();
        let Some(store) = &mut self.user_store else {
            return;
        };
        self.user.prune(now);
        let written = encode_user_state(&store.user, &self.user, core.correct_pc_errata(), now)
            .and_then(|bytes| Ok(store.record.write(&bytes)?));
        match written {
            Ok(()) => store.status = format!("User {} · state saved", store.user),
            Err(error) => {
                warn!(
                    "S-124 user state not saved to {}: {error:#}",
                    store.record.describe()
                );
                store.status = format!("User {} · state NOT saved: {error}", store.user);
            }
        }
        core.ui_revision = 0;
    }
}

impl ProductAdapter for S124Overlay {
    fn info(&self) -> ProductInfo {
        ProductInfo {
            id: crate::PRODUCT,
            title: "Navigational Warnings",
            encoding: Encoding::Gml,
            role: SceneRole::Overlay,
        }
    }
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised> {
        if ferrite_s100_gml::overlay::sniff(name, head, &[&crate::rules::IDENTITY]).is_some() {
            return Some(Recognised::Dataset);
        }
        let named = name
            .extension()
            .is_some_and(|e| IMPORT_EXTENSIONS.iter().any(|i| e.eq_ignore_ascii_case(i)));
        named
            .then(|| crate::import::ImportFormat::detect(name, head))
            .flatten()
            .map(|format| Recognised::Import(format.label()))
    }
    fn import_extensions(&self) -> &'static [&'static str] {
        &IMPORT_EXTENSIONS
    }
    fn imports_archives(&self) -> bool {
        true
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

impl GmlOverlayProduct for S124Overlay {
    fn rules(&self) -> &'static dyn GmlRules {
        &S124
    }

    fn undeclared_viewing_groups(&self) -> &'static [u32] {
        &IMPLICIT_GROUPS
    }

    fn errata_correction(&self) -> Option<&'static str> {
        Some("S-124 undeclared viewing groups drawn")
    }

    fn portrayal_settings(
        &self,
        errata: bool,
        implicit_groups: &HashSet<String>,
    ) -> PortrayalSettings {
        PortrayalSettings {
            key: 0b11 | (errata as u64) << 2,
            context: Vec::new(),
            convert: ConvertOptions {
                implicit_viewing_groups: if errata {
                    implicit_groups.clone()
                } else {
                    HashSet::new()
                },
                ..ConvertOptions::default()
            },
        }
    }

    fn import(
        &self,
        path: &Path,
        bytes: &[u8],
        fc: Option<&FeatureCatalogue>,
        now: DateTime<Utc>,
        local_paths: bool,
    ) -> anyhow::Result<(Dataset, &'static str)> {
        use anyhow::Context;
        let format = crate::import::ImportFormat::detect(path, bytes)
            .context("Unrecognised import format")?;
        // Shapefile companions are found beside the file: native only.
        let dataset = if local_paths {
            crate::import::import_path_with(path, fc, now)?
        } else {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("import");
            let mut dataset = crate::import::import_bytes(bytes, format, name, fc, now)
                .with_context(|| path.display().to_string())?;
            dataset.source = Some(path.to_path_buf());
            dataset
        };
        Ok((dataset, format.label()))
    }

    /// The first receipt survives restarts (11.1.1 c: 24 h indication).
    fn received(&mut self, core: &mut OverlayCore, dataset: &Dataset) -> DateTime<Utc> {
        let (received, recorded) = self
            .user
            .receive(&navwarn::dataset_key(dataset), core.now());
        if recorded {
            self.persist_user_state(core);
        }
        received
    }

    /// Datasets whose message status is Cancelled (any reason).
    fn withdrawn(&mut self, core: &mut OverlayCore) -> HashSet<u32> {
        if !core.datasets.iter().any(|d| d.product() == crate::PRODUCT) {
            return HashSet::new();
        }
        self.registry(core);
        let registry = self.registry.as_ref().expect("evaluated");
        registry
            .messages
            .iter()
            .zip(&self.registry_ids)
            .filter(|(m, _)| m.status.is_cancelled())
            .map(|(_, id)| *id)
            .collect()
    }

    fn invalidate(&mut self) {
        self.registry = None;
    }

    /// Evaluate the S-124 lifecycle now, or reuse the evaluation of the last
    /// minute. Returns true when the result differs from the previous one.
    fn evaluate(&mut self, core: &mut OverlayCore, force: bool) -> bool {
        if !force
            && self.registry.is_some()
            && self
                .evaluated
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60))
        {
            return false;
        }
        let ids: Vec<u32> = core
            .datasets
            .iter()
            .filter(|d| d.product() == crate::PRODUCT)
            .map(|d| d.id)
            .collect();
        let inputs: Vec<NavwarnInput> = core
            .datasets
            .iter()
            .filter(|d| d.product() == crate::PRODUCT)
            .map(|d| NavwarnInput {
                dataset: &d.dataset,
                received: d.received,
            })
            .collect();
        let registry = navwarn::evaluate(&inputs, core.lifecycle_time());
        let signature = |r: &NavwarnRegistry| -> Vec<(Status, Vec<String>)> {
            r.messages
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let mut suppress: Vec<String> = r
                        .feature_filter(i, &self.user, &self.policy, &[])
                        .suppress
                        .into_iter()
                        .collect();
                    suppress.sort();
                    (m.status.clone(), suppress)
                })
                .collect()
        };
        let changed = self.registry_ids != ids
            || self
                .registry
                .as_ref()
                .is_none_or(|old| signature(old) != signature(&registry));
        let clock = core.now();
        let new_flags: Vec<bool> = (0..registry.messages.len())
            .map(|i| {
                let meets = self.user.classification_of(&registry.messages[i])
                    != UserClassification::Information;
                registry.is_new_at(i, &self.user, meets, clock)
            })
            .collect();
        if new_flags != self.new_flags {
            // The 24 h new-message indication expired (list only).
            self.new_flags = new_flags;
            core.ui_revision = 0;
        }
        self.registry = Some(registry);
        self.registry_ids = ids;
        self.evaluated = Some(ferrite_kernel::time::Instant::now());
        if changed && !force {
            core.bump_revision();
        }
        changed
    }

    fn ensure_evaluated(&mut self, core: &mut OverlayCore) {
        self.registry(core);
    }

    fn next_check(&self) -> Option<ferrite_kernel::time::Instant> {
        let now = ferrite_kernel::time::Instant::now();
        Some(
            self.evaluated
                .and_then(|t| t.checked_add(std::time::Duration::from_secs(60)))
                .unwrap_or(now),
        )
    }

    fn suppressed(&self, core: &OverlayCore, id: u32) -> Option<HashSet<String>> {
        let selected: Vec<&str> = core
            .list_selection
            .iter()
            .filter(|(d, _)| *d == id)
            .map(|(_, f)| f.as_str())
            .collect();
        let index = self.registry_index(id)?;
        Some(
            self.registry
                .as_ref()
                .expect("evaluated")
                .feature_filter(index, &self.user, &self.policy, &selected)
                .suppress,
        )
    }

    fn describe(&self, _core: &OverlayCore, id: u32) -> Vec<(String, String)> {
        let mut rows = Vec::new();
        if let Some(m) = self
            .registry_index(id)
            .and_then(|i| self.registry.as_ref()?.message(i))
        {
            rows.push(("Warning".into(), m.caption()));
            rows.push(("Warning status".into(), m.status.label().into()));
            if let Some(t) = m.publication_time {
                rows.push(("Publication time".into(), format_time(t)));
            }
        }
        rows
    }

    /// S-124 parts: the preamble carries the message identity.
    fn related_information(&self) -> &'static [(&'static str, &'static str)] {
        &[("NavwarnPreamble", "Preamble › ")]
    }

    /// The features [`navwarn::FeatureFilter::highlight`] names (the
    /// NavwarnPart and its NavwarnAreaAffected).
    fn highlight(&self, _core: &OverlayCore, id: u32, feature: &str) -> Vec<String> {
        let Some(index) = self.registry_index(id) else {
            return Vec::new();
        };
        self.registry
            .as_ref()
            .expect("evaluated")
            .feature_filter(index, &self.user, &self.policy, &[feature])
            .highlight
            .into_iter()
            .collect()
    }

    /// A message part with its associated features.
    fn part_objects(
        &self,
        _core: &OverlayCore,
        id: u32,
        part: &str,
    ) -> Option<Option<HashSet<String>>> {
        let Some(index) = self.registry_index(id) else {
            return Some(None);
        };
        let m = self.registry.as_ref()?.message(index)?;
        let p = m.parts.iter().find(|p| p.id == part)?;
        Some(Some(
            std::iter::once(p.id.clone())
                .chain(p.areas_affected.iter().cloned())
                .chain(p.text_placements.iter().cloned())
                .collect(),
        ))
    }

    /// First NavwarnPart with geometry of a message.
    fn first_part(&self, _core: &OverlayCore, id: u32) -> Option<String> {
        let index = self.registry_index(id)?;
        let m = self.registry.as_ref()?.message(index)?;
        m.parts
            .iter()
            .find(|p| p.has_geometry)
            .map(|p| p.id.clone())
    }

    fn selected(&mut self, core: &mut OverlayCore, id: u32) {
        self.mark_displayed(core, id);
    }

    fn errata_changed(&mut self, core: &mut OverlayCore) {
        self.persist_user_state(core);
    }

    /// Load the persisted user state from `record` and keep saving it there.
    /// A missing record starts empty; an unreadable one is set aside and the
    /// application starts with an empty state (never fails).
    fn attach_user_record(
        &mut self,
        core: &mut OverlayCore,
        record: Box<dyn UserRecord>,
        user: String,
    ) -> Option<bool> {
        let mut errata = None;
        let status = match read_user_record(record.as_ref()) {
            Ok(Some(StoredUserState {
                state: stored,
                user: stored_user,
                correct_pc_errata,
            })) => {
                if stored_user != user {
                    warn!(
                        "S-124 user state {} was saved by user {stored_user:?}, loaded for {user:?}",
                        record.describe()
                    );
                }
                // Loaded state first; anything recorded this session wins.
                let mut merged = stored;
                merged
                    .classification
                    .extend(self.user.classification.drain());
                merged.displayed.extend(self.user.displayed.drain());
                for (k, t) in self.user.received.drain() {
                    let e = merged.received.entry(k).or_insert(t);
                    *e = (*e).min(t);
                }
                self.user = merged;
                errata = Some(correct_pc_errata);
                info!(
                    "S-124 user state for {user}: {} classifications, {} displayed, {} receipts ({})",
                    self.user.classification.len(),
                    self.user.displayed.len(),
                    self.user.received.len(),
                    record.describe()
                );
                format!("User {user} · state restored")
            }
            Ok(None) => format!("User {user} · new state"),
            Err(error) => {
                warn!(
                    "S-124 user state {} unreadable, starting empty: {error:#}",
                    record.describe()
                );
                set_aside_record(record.as_ref(), core.now());
                format!("User {user} · saved state unreadable, started empty")
            }
        };
        // Datasets opened before the store was attached keep their earliest receipt.
        for d in core
            .datasets
            .iter_mut()
            .filter(|d| d.product() == crate::PRODUCT)
        {
            let (received, _) = self
                .user
                .receive(&navwarn::dataset_key(&d.dataset), d.received);
            d.received = received;
        }
        self.user_store = Some(UserStateStore {
            record,
            user,
            status,
        });
        errata
    }
}

pub fn timing_label(t: PartTiming) -> String {
    match t {
        PartTiming::NoRange => "in force".into(),
        PartTiming::InRange => "in its date range".into(),
        PartTiming::BeforeRange { starts } => format!("starts {}", format_time(starts)),
        PartTiming::AfterRange { ended } => format!("ended {}", format_time(ended)),
        PartTiming::OutsideRecurring => "outside its recurring dates".into(),
    }
}

pub fn cancel_reason(reason: &CancelReason, registry: &NavwarnRegistry) -> String {
    let caption = |i: usize| {
        registry
            .message(i)
            .map(|m| m.caption())
            .unwrap_or_else(|| format!("dataset {i}"))
    };
    match reason {
        CancelReason::CancellationDate(t) => {
            format!("cancellation date {}", format_time(*t))
        }
        CancelReason::CancelledBy { dataset } => format!("by {}", caption(*dataset)),
        CancelReason::NotInForceBulletin { bulletin } => {
            format!("not in force per {}", caption(*bulletin))
        }
        CancelReason::SupersededInForceBulletin { by } => {
            format!("superseded by {}", caption(*by))
        }
        CancelReason::ReplacedByAuthoritative { by } => {
            format!("replaced by official {}", caption(*by))
        }
    }
}
