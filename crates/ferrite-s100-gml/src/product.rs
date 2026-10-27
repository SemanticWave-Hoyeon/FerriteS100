//! The interface of a GML (S-100 Part 10b) product module.
//!
//! Shared GML infrastructure (reader, feature-catalogue validation,
//! exchange-set validation, Part 9 XSLT portrayal input and conversion, the
//! overlay layer state) is product-neutral. A product crate (`ferrite-s124`,
//! `ferrite-s129`) supplies:
//!
//! * [`GmlRules`]: the product's static rules, a `static` value: validation
//!   check identifiers and product rules, exchange-set rules, file naming,
//!   imports, and the factory of its overlay module.
//! * [`GmlOverlayProduct`]: the product's overlay module, owned by the
//!   engine's overlay host: portrayal parameters, lifecycle and per-user
//!   state, feature filtering, picking and highlight details. Every hook has
//!   a neutral default, so a product implements only what it adds.
use crate::exchange::ExchangeRule;
use crate::model::{Dataset, Product, Severity};
use crate::overlay::OverlayCore;
use crate::portrayal::{CatalogueInfo, ConvertOptions, InputOptions};
use crate::validate::{FcRule, Incoming, IssueSink, Validator};
use chrono::{DateTime, Utc};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_product_api::platform::UserRecord;
use ferrite_product_api::ProductAdapter;
use std::collections::HashSet;
use std::path::Path;

/// Static rules and factory of a GML product.
pub trait GmlRules: Send + Sync {
    fn product(&self) -> Product;

    // ── Dataset validation ([`crate::validate`]) ─────────────────────────

    /// Check identifier and severity of a generic feature-catalogue rule.
    fn fc_rule_check(&self, rule: FcRule) -> (String, Severity);
    /// The product's dataset file naming convention.
    fn file_name_ok(&self, name: &str) -> bool;
    /// Association bindings (class, role, target class) whose multiplicity
    /// the product checks itself instead of the generic check.
    fn checked_by_product(&self, _class: &str, _role: &str, _target: &str) -> bool {
        false
    }
    /// Dataset-level product checks, after the generic dataset checks.
    fn dataset_checks(&self, _dataset: &Dataset, _byte_size: Option<u64>, _sink: &mut IssueSink) {}
    /// Product rules, after the generic object checks.
    fn product_checks(
        &self,
        _validator: &Validator,
        _dataset: &Dataset,
        _byte_size: Option<u64>,
        _incoming: &Incoming,
        _sink: &mut IssueSink,
    ) {
    }

    // ── Exchange sets ([`crate::exchange`]) ──────────────────────────────

    /// Check identifier and severity of an exchange-set rule.
    fn exchange_check(&self, rule: ExchangeRule) -> (String, Severity);
    /// Mandatory dataset discovery metadata.
    fn mandatory_metadata(&self) -> &'static [&'static str];
    /// Permitted dataset discovery purposes.
    fn purposes(&self) -> &'static [&'static str];
    /// Whether a numeric purpose code is permitted.
    fn numeric_purpose_ok(&self, purpose: &str) -> bool;
    /// Dataset size limit (bytes).
    fn max_dataset_bytes(&self) -> u64;
    /// Finding text of a dataset over [`GmlRules::max_dataset_bytes`].
    fn size_message(&self, size: u64) -> String;
    /// Finding of a support file with this resource purpose, if not allowed.
    fn support_file_issue(&self, _resource_purpose: Option<&str>) -> Option<String> {
        None
    }
    /// Finding of a dataProtection flag, if not allowed.
    fn data_protection_issue(&self, _protected: Option<bool>) -> Option<String> {
        None
    }

    // ── Module ───────────────────────────────────────────────────────────

    /// Lower-case extensions of non-S-100 sources the product imports.
    fn import_extensions(&self) -> &'static [&'static str] {
        &[]
    }
    /// Create the product's overlay module.
    fn overlay(&self) -> Box<dyn GmlOverlayProduct>;
}

/// How a product's datasets are portrayed with the current options.
pub struct PortrayalSettings {
    /// Equal keys give equal portrayals (the host reuses its XSLT result).
    pub key: u64,
    /// Context parameter overrides of the XSLT rules.
    pub context: Vec<(&'static str, &'static str)>,
    pub convert: ConvertOptions,
}

/// A GML product's overlay module.
///
/// Hooks receive the layer state ([`OverlayCore`]): the loaded datasets of
/// every product (a product looks only at its own), the viewing time, the
/// platform clock and the shared options. Dataset ids are the core's.
pub trait GmlOverlayProduct: ProductAdapter {
    fn rules(&self) -> &'static dyn GmlRules;
    fn product(&self) -> Product {
        self.rules().product()
    }

    // ── Catalogues and portrayal ─────────────────────────────────────────

    /// Viewing groups the product's rules use but its portrayal catalogue
    /// does not declare; literally (Part 9 9-11.2.2) they are never drawn,
    /// the "Correct known PC errata" option draws them.
    fn undeclared_viewing_groups(&self) -> &'static [u32] {
        &[]
    }
    /// The product's catalogues were loaded (context parameter defaults,
    /// conformance notes).
    fn catalogues_loaded(&mut self, _info: &CatalogueInfo, _pc_version: &str) {}
    /// What the "Correct known PC errata" option corrects in this product.
    fn errata_correction(&self) -> Option<&'static str> {
        None
    }
    /// Options of the Part 9-A portrayal input (virtual features).
    fn input_options(&self) -> InputOptions {
        InputOptions::default()
    }
    /// Context parameters and conversion options; `implicit_groups` are the
    /// undeclared viewing groups of the loaded catalogue.
    fn portrayal_settings(
        &self,
        errata: bool,
        implicit_groups: &HashSet<String>,
    ) -> PortrayalSettings;

    // ── Datasets ─────────────────────────────────────────────────────────

    /// Convert a non-S-100 source (`path` with its `bytes`; `local_paths`:
    /// companion files may be read beside it) with the product's feature
    /// catalogue; returns the dataset and the format label.
    fn import(
        &self,
        _path: &Path,
        _bytes: &[u8],
        _fc: Option<&FeatureCatalogue>,
        _now: DateTime<Utc>,
        _local_paths: bool,
    ) -> anyhow::Result<(Dataset, &'static str)> {
        anyhow::bail!("{} imports no other formats", self.product().identifier())
    }
    /// First receipt time of a dataset being added.
    fn received(&mut self, core: &mut OverlayCore, _dataset: &Dataset) -> DateTime<Utc> {
        core.now()
    }
    /// Ids of datasets no longer on the system although loaded (S-124
    /// cancelled messages), for exchange-set cancellation checks.
    fn withdrawn(&mut self, _core: &mut OverlayCore) -> HashSet<u32> {
        HashSet::new()
    }

    // ── Lifecycle ────────────────────────────────────────────────────────

    /// Datasets or options changed: drop derived state.
    fn invalidate(&mut self) {}
    /// Re-evaluate time-dependent state (or reuse a recent evaluation when
    /// not `force`d); true when the result changed.
    fn evaluate(&mut self, _core: &mut OverlayCore, _force: bool) -> bool {
        false
    }
    /// Evaluate when nothing is evaluated since the last invalidation.
    fn ensure_evaluated(&mut self, _core: &mut OverlayCore) {}
    /// When the next periodic evaluation is due.
    fn next_check(&self) -> Option<ferrite_kernel::time::Instant> {
        None
    }
    /// Feature ids of a dataset not to draw now; None: the dataset is not
    /// portrayed. Called after [`GmlOverlayProduct::ensure_evaluated`].
    fn suppressed(&self, _core: &OverlayCore, _id: u32) -> Option<HashSet<String>> {
        Some(HashSet::new())
    }

    // ── Picking ──────────────────────────────────────────────────────────

    /// Rows of the feature report after the dataset, product and
    /// provenance rows.
    fn describe(&self, _core: &OverlayCore, _id: u32) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Information types (code, row prefix) whose attributes are reported
    /// with a feature that references them.
    fn related_information(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }
    /// Object ids to highlight with a selected feature.
    fn highlight(&self, _core: &OverlayCore, _id: u32, _feature: &str) -> Vec<String> {
        Vec::new()
    }
    /// Object ids of a part of a dataset chosen in a list, for focusing:
    /// `Some(None)` focuses the whole dataset, None nothing (unknown part).
    fn part_objects(
        &self,
        _core: &OverlayCore,
        _id: u32,
        _part: &str,
    ) -> Option<Option<HashSet<String>>> {
        Some(None)
    }
    /// The part a list selection of the dataset focuses first.
    fn first_part(&self, _core: &OverlayCore, _id: u32) -> Option<String> {
        None
    }
    /// A feature of the dataset was selected or the dataset chosen in a list.
    fn selected(&mut self, _core: &mut OverlayCore, _id: u32) {}

    // ── Per-user state ───────────────────────────────────────────────────

    /// The "Correct known PC errata" option changed (persist it).
    fn errata_changed(&mut self, _core: &mut OverlayCore) {}
    /// Keep the product's per-user state in `record` for OS user `user`;
    /// returns the stored errata option when a state was restored.
    fn attach_user_record(
        &mut self,
        _core: &mut OverlayCore,
        _record: Box<dyn UserRecord>,
        _user: String,
    ) -> Option<bool> {
        None
    }
}
