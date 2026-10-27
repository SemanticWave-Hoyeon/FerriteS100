//! S-100 Part 17 exchange-set validation for GML products.
//!
//! Evaluates the exchange-set level checks the single-dataset validator
//! cannot: references, discovery metadata, purposes, sizes and
//! cancellations. The registered products' [`GmlRules`] map each rule
//! ([`ExchangeRule`]) to their check identifier and severity and supply the
//! product's metadata, purpose, size and support-file rules (S-158-129
//! checks 11, 17-20 for S-129; S-124 clauses 9.5-9.8 and 12).
//! The exchange catalogue (CATALOG.XML, any S-100 XC edition) is read by
//! local element names. Datasets referenced by the catalogue are read
//! without a feature catalogue to compare their content with the metadata.
use crate::model::{Attribute, Dataset, Issue, Product, Value};
use crate::product::GmlRules;
use crate::reader::{read_file, ReadLimits};
use anyhow::{ensure, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 100_000;
const MAX_CATALOGUE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DATASET_BYTES: usize = 64 * 1024 * 1024;

/// One `S100_DatasetDiscoveryMetadata` record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DatasetDiscovery {
    /// The `fileName` URI as encoded (for example `file:/S-124/DATASET_FILES/…`).
    pub file_name: String,
    /// Local names of the populated child elements.
    pub fields: HashSet<String>,
    pub purpose: Option<String>,
    pub edition_number: Option<String>,
    pub update_number: Option<String>,
    pub product_identifier: Option<String>,
    pub product_version: Option<String>,
    pub issue_date: Option<String>,
    /// West, south, east, north.
    pub bounding_box: Option<[f64; 4]>,
    pub temporal_begin: Option<String>,
    pub temporal_end: Option<String>,
    pub encoding_format: Option<String>,
    pub data_protection: Option<bool>,
}

impl DatasetDiscovery {
    /// Fileless cancellation (S-100 17-4.4.1): purpose `cancellation`, or
    /// edition 0 (S-129 19.1 note 2).
    pub fn is_cancellation(&self) -> bool {
        matches!(self.purpose.as_deref(), Some("cancellation" | "5"))
            || self.edition_number.as_deref() == Some("0")
    }
}

/// One `S100_CatalogueDiscoveryMetadata` record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogueDiscovery {
    pub file_name: String,
    pub purpose: Option<String>,
    pub scope: Option<String>,
    pub product_identifier: Option<String>,
}

/// One `S100_SupportFileDiscoveryMetadata` record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportFileDiscovery {
    pub file_name: String,
    pub revision_status: Option<String>,
    pub resource_purpose: Option<String>,
    pub data_type: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExchangeCatalogue {
    pub datasets: Vec<DatasetDiscovery>,
    pub catalogues: Vec<CatalogueDiscovery>,
    pub support_files: Vec<SupportFileDiscovery>,
}

type Node<'a, 'i> = roxmltree::Node<'a, 'i>;

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}
/// The first non-empty text below the node (handles gco:CharacterString wrappers).
fn deep_text(n: Node) -> Option<String> {
    n.descendants()
        .filter(|d| d.is_text())
        .filter_map(|d| d.text())
        .map(str::trim)
        .find(|t| !t.is_empty())
        .map(str::to_owned)
}
fn field(n: Node, name: &str) -> Option<String> {
    child(n, name).and_then(deep_text)
}

/// Parses an exchange catalogue (S-100 XC 5.x) by local element names.
pub fn parse_catalogue(bytes: &[u8]) -> Result<ExchangeCatalogue> {
    let text = std::str::from_utf8(bytes).context("Exchange catalogue is not UTF-8")?;
    let doc = roxmltree::Document::parse_with_options(
        text.trim_start_matches('\u{feff}'),
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 10_000_000,
        },
    )
    .context("Exchange catalogue is not well-formed")?;
    let root = doc.root_element();
    ensure!(
        root.tag_name().name() == "S100_ExchangeCatalogue",
        "Root element is {}, not S100_ExchangeCatalogue",
        root.tag_name().name()
    );
    let mut out = ExchangeCatalogue::default();
    for n in root.descendants().filter(|n| n.is_element()) {
        match n.tag_name().name() {
            "S100_DatasetDiscoveryMetadata" => {
                let spec = child(n, "productSpecification");
                let temporal = child(n, "temporalExtent");
                let bbox = child(n, "boundingBox").and_then(|b| {
                    let v = |name: &str| {
                        b.descendants()
                            .find(|d| d.is_element() && d.tag_name().name() == name)
                            .and_then(deep_text)?
                            .parse::<f64>()
                            .ok()
                    };
                    Some([
                        v("westBoundLongitude")?,
                        v("southBoundLatitude")?,
                        v("eastBoundLongitude")?,
                        v("northBoundLatitude")?,
                    ])
                });
                out.datasets.push(DatasetDiscovery {
                    file_name: field(n, "fileName").unwrap_or_default(),
                    fields: n
                        .children()
                        .filter(|c| c.is_element() && deep_text(*c).is_some())
                        .map(|c| c.tag_name().name().to_string())
                        .collect(),
                    purpose: field(n, "purpose"),
                    edition_number: field(n, "editionNumber"),
                    update_number: field(n, "updateNumber"),
                    product_identifier: spec.and_then(|s| field(s, "productIdentifier")),
                    product_version: spec.and_then(|s| field(s, "version")),
                    issue_date: field(n, "issueDate"),
                    bounding_box: bbox,
                    temporal_begin: temporal.and_then(|t| field(t, "timeInstantBegin")),
                    temporal_end: temporal.and_then(|t| field(t, "timeInstantEnd")),
                    encoding_format: field(n, "encodingFormat"),
                    data_protection: field(n, "dataProtection").map(|v| v == "true" || v == "1"),
                });
            }
            "S100_CatalogueDiscoveryMetadata" => out.catalogues.push(CatalogueDiscovery {
                file_name: field(n, "fileName").unwrap_or_default(),
                purpose: field(n, "purpose"),
                scope: field(n, "scope"),
                product_identifier: child(n, "productSpecification")
                    .and_then(|s| field(s, "productIdentifier")),
            }),
            "S100_SupportFileDiscoveryMetadata" => out.support_files.push(SupportFileDiscovery {
                file_name: field(n, "fileName").unwrap_or_default(),
                revision_status: field(n, "revisionStatus"),
                resource_purpose: field(n, "resourcePurpose"),
                data_type: field(n, "dataType"),
            }),
            _ => {}
        }
    }
    Ok(out)
}

/// Exchange-set rules, mapped per product ([`GmlRules::exchange_check`]) to
/// a check identifier and severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeRule {
    /// CATALOG.XML missing or unreadable.
    Catalogue,
    /// CATALOG.XML misnamed, or another file named CATALOG.XML.
    CatalogueName,
    Signature,
    FileName,
    /// 17: a referenced file is not in the exchange set.
    Reference,
    /// 18: metadata does not correspond to the dataset or the content table.
    Metadata,
    /// 18: a mandatory metadata element is missing (warning for S-124).
    MetadataMissing,
    /// A dataset file without discovery metadata.
    UnlistedDataset,
    UnlistedCatalogue,
    UnlistedSupport,
    /// S-124 9.5: only language packs are support files.
    SupportFile,
    Purpose,
    Size,
    /// 19: cancellation of a dataset not on the system or already cancelled.
    CancelUnknown,
    /// 20: cancellation with a dataset file in the exchange set.
    CancelWithFile,
}

/// Every S-100 dataset discovery purpose.
pub const PURPOSES: [&str; 6] = [
    "newDataset",
    "newEdition",
    "update",
    "reissue",
    "cancellation",
    "delta",
];

/// Result of validating one exchange set.
#[derive(Debug, Clone, Default)]
pub struct ExchangeSetReport {
    /// The S100_ROOT folder that was validated.
    pub root: PathBuf,
    /// S-124 / S-129 content found in the exchange set.
    pub products: Vec<Product>,
    pub catalogue: Option<ExchangeCatalogue>,
    pub issues: Vec<Issue>,
}

struct Sink<'a> {
    issues: &'a mut Vec<Issue>,
}

impl Sink<'_> {
    fn push(
        &mut self,
        product: &dyn GmlRules,
        rule: ExchangeRule,
        object: Option<&str>,
        message: String,
    ) {
        let (check, severity) = product.exchange_check(rule);
        self.issues.push(Issue {
            severity,
            check,
            object: object.map(str::to_owned),
            message,
        });
    }
}

/// Files of the exchange set: relative path ('/' separated) and size.
struct Inventory {
    files: Vec<(String, u64)>,
    by_lower: HashMap<String, usize>,
    by_base: HashMap<String, Vec<usize>>,
}

impl Inventory {
    fn scan(root: &Path) -> Result<Self> {
        let mut files = Vec::new();
        let mut stack = vec![(root.to_path_buf(), String::new())];
        let mut count = 0usize;
        while let Some((dir, prefix)) = stack.pop() {
            for entry in std::fs::read_dir(&dir).with_context(|| dir.display().to_string())? {
                count += 1;
                ensure!(
                    count <= MAX_ENTRIES,
                    "Exchange set exceeds {MAX_ENTRIES} entries"
                );
                let entry = entry?;
                // file_type does not follow symbolic links; links are ignored.
                let kind = entry.file_type()?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let rel = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if kind.is_dir() {
                    stack.push((entry.path(), rel));
                } else if kind.is_file() {
                    files.push((rel, entry.metadata()?.len()));
                }
            }
        }
        files.sort();
        let mut by_lower = HashMap::new();
        let mut by_base: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, (rel, _)) in files.iter().enumerate() {
            by_lower.insert(rel.to_ascii_lowercase(), i);
            by_base
                .entry(base_name(rel).to_ascii_lowercase())
                .or_default()
                .push(i);
        }
        Ok(Self {
            files,
            by_lower,
            by_base,
        })
    }

    /// Index of the file a relative path (or a bare file name) refers to.
    fn find(&self, rel: &str) -> Option<usize> {
        let lower = rel.to_ascii_lowercase();
        if let Some(i) = self.by_lower.get(&lower) {
            return Some(*i);
        }
        if !rel.contains('/') {
            return self.by_base.get(&lower).and_then(|v| v.first().copied());
        }
        None
    }
}

fn base_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// The exchange-set relative path of an S-100 file URI (S-100 1-4.6), for
/// example `file:/S-124/DATASET_FILES/124GB00X.GML` or `file::124GB00X.TXT`.
/// None for empty paths and paths that leave the exchange set.
fn resolve(uri: &str) -> Option<String> {
    let mut p = uri.trim();
    if p.len() >= 5 && p[..5].eq_ignore_ascii_case("file:") {
        p = &p[5..];
    }
    let p = p.trim_start_matches([':', '/', '\\']).replace('\\', "/");
    let segments: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty()
        || segments
            .iter()
            .any(|s| *s == "." || *s == ".." || s.contains(':'))
    {
        return None;
    }
    Some(segments.join("/"))
}

/// The product of a catalogue record: product specification, then the
/// product folder, then the file name prefix (of the `known` products).
fn product_of(identifier: Option<&str>, rel: Option<&str>, known: &[Product]) -> Option<Product> {
    if let Some(p) = identifier {
        return Product::from_identifier(p);
    }
    let rel = rel?;
    if let Some(p) = rel.split('/').next().and_then(Product::from_identifier) {
        if rel.contains('/') {
            return Some(p);
        }
    }
    let base = base_name(rel);
    known.iter().copied().find(|p| base.starts_with(p.code()))
}

fn parse_int(v: Option<&str>) -> Option<i64> {
    v?.trim().parse().ok()
}

fn parse_time(v: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let v = v.trim();
    chrono::DateTime::parse_from_rfc3339(v).ok().or_else(|| {
        chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S")
            .ok()
            .map(|t| t.and_utc().fixed_offset())
    })
}

/// File references in dataset content: `fileReference` values and
/// `file:` URIs at any attribute depth.
fn file_references(ds: &Dataset) -> Vec<String> {
    fn walk(attrs: &[Attribute], out: &mut Vec<String>) {
        for a in attrs {
            match &a.value {
                Value::Simple { text, .. } => {
                    let t = text.trim();
                    let is_uri = t.len() >= 5 && t[..5].eq_ignore_ascii_case("file:");
                    if (a.code == "fileReference" && !t.is_empty()) || is_uri {
                        out.push(t.to_string());
                    }
                }
                Value::Complex(children) => walk(children, out),
                Value::Nil { .. } => {}
            }
        }
    }
    let mut out = Vec::new();
    for o in &ds.objects {
        walk(&o.attributes, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// The S100_ROOT folder for a selected folder (the root itself, or its
/// `S100_ROOT` child when the folder has no catalogue).
pub fn exchange_root(folder: &Path) -> PathBuf {
    let has_catalogue = |dir: &Path| {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("CATALOG.XML")
            })
        })
    };
    let nested = folder.join("S100_ROOT");
    if !has_catalogue(folder) && nested.is_dir() {
        nested
    } else {
        folder.to_path_buf()
    }
}

/// Validates the content of an exchange set that belongs to the `products`
/// (the registered GML products; content of other products is ignored).
///
/// `folder` is the S100_ROOT folder (or its parent). `on_system` holds the
/// file names (any case) of the datasets currently loaded and not cancelled;
/// the check of cancellations of unknown datasets needs it and is skipped
/// when it is None. Returns an empty report when the folder holds no content
/// of the products.
pub fn validate_exchange_set(
    folder: &Path,
    on_system: Option<&HashSet<String>>,
    products: &[&dyn GmlRules],
) -> Result<ExchangeSetReport> {
    let known: Vec<Product> = products.iter().map(|r| r.product()).collect();
    let rules_of = |p: Product| -> &dyn GmlRules {
        *products
            .iter()
            .find(|r| r.product() == p)
            .expect("registered product")
    };
    let product_of = |identifier: Option<&str>, rel: Option<&str>| {
        product_of(identifier, rel, &known).filter(|p| known.contains(p))
    };
    let root = exchange_root(folder);
    let inventory = Inventory::scan(&root)?;
    let mut report = ExchangeSetReport {
        root: root.clone(),
        ..Default::default()
    };

    // The exchange catalogue: exactly one, CATALOG.XML in the root.
    let root_catalogue = inventory
        .files
        .iter()
        .position(|(rel, _)| !rel.contains('/') && rel.eq_ignore_ascii_case("CATALOG.XML"));
    let mut catalogue_error = None;
    if let Some(i) = root_catalogue {
        let (rel, size) = &inventory.files[i];
        if *size > MAX_CATALOGUE_BYTES {
            catalogue_error = Some(format!("{rel} exceeds {MAX_CATALOGUE_BYTES} bytes"));
        } else {
            match std::fs::read(root.join(rel))
                .context("Cannot read the exchange catalogue")
                .and_then(|b| parse_catalogue(&b))
            {
                Ok(c) => report.catalogue = Some(c),
                Err(e) => catalogue_error = Some(format!("{e:#}")),
            }
        }
    }

    // Products in the exchange set.
    let mut products: Vec<Product> = Vec::new();
    let mut add = |p: Option<Product>| {
        if let Some(p) = p {
            if !products.contains(&p) {
                products.push(p);
            }
        }
    };
    if let Some(c) = &report.catalogue {
        for d in &c.datasets {
            add(product_of(
                d.product_identifier.as_deref(),
                resolve(&d.file_name).as_deref(),
            ));
        }
    }
    for (rel, _) in &inventory.files {
        if rel.contains('/') {
            add(rel
                .split('/')
                .next()
                .and_then(Product::from_identifier)
                .filter(|p| known.contains(p)));
        }
    }
    products.sort_by_key(|p| p.code());
    report.products = products.clone();
    if products.is_empty() {
        return Ok(report);
    }

    let mut sink = Sink {
        issues: &mut report.issues,
    };
    let all = |sink: &mut Sink, rule: ExchangeRule, object: Option<&str>, message: &str| {
        for p in &products {
            sink.push(rules_of(*p), rule, object, message.to_string());
        }
    };
    match (root_catalogue, &catalogue_error) {
        (None, _) => all(
            &mut sink,
            ExchangeRule::Catalogue,
            None,
            "The exchange set has no exchange catalogue (CATALOG.XML)",
        ),
        (Some(_), Some(e)) => all(
            &mut sink,
            ExchangeRule::Catalogue,
            Some("CATALOG.XML"),
            &format!("The exchange catalogue cannot be read: {e}"),
        ),
        (Some(i), None) if inventory.files[i].0 != "CATALOG.XML" => all(
            &mut sink,
            ExchangeRule::CatalogueName,
            Some(&inventory.files[i].0),
            "The exchange catalogue must be named CATALOG.XML",
        ),
        _ => {}
    }
    for (rel, _) in &inventory.files {
        if rel.contains('/') && base_name(rel).eq_ignore_ascii_case("CATALOG.XML") {
            all(
                &mut sink,
                ExchangeRule::CatalogueName,
                Some(rel),
                "No file other than the exchange catalogue may be named CATALOG.XML",
            );
        }
    }
    if !inventory
        .files
        .iter()
        .any(|(rel, _)| rel.eq_ignore_ascii_case("CATALOG.SIGN"))
    {
        all(
            &mut sink,
            ExchangeRule::Signature,
            None,
            "The exchange catalogue signature (CATALOG.SIGN) is missing",
        );
    }

    // Files described by the catalogue.
    let mut listed: HashSet<usize> = HashSet::new();
    if let Some(i) = root_catalogue {
        listed.insert(i);
    }
    if let Some(catalogue) = report.catalogue.as_ref() {
        let mut cancelled: HashSet<String> = HashSet::new();
        for d in &catalogue.datasets {
            let rel = resolve(&d.file_name);
            let Some(product) = product_of(d.product_identifier.as_deref(), rel.as_deref()) else {
                continue;
            };
            let Some(rel) = rel else {
                sink.push(
                    rules_of(product),
                    ExchangeRule::Reference,
                    None,
                    format!("Dataset fileName '{}' is not a valid file URI", d.file_name),
                );
                continue;
            };
            let name = base_name(&rel).to_string();
            let object = Some(name.as_str());
            if !rules_of(product).file_name_ok(&name) {
                sink.push(
                    rules_of(product),
                    ExchangeRule::FileName,
                    object,
                    format!(
                        "Dataset file name does not follow the {} naming convention",
                        product.identifier()
                    ),
                );
            }
            let found = inventory.find(&rel);
            if let Some(i) = found {
                listed.insert(i);
            }
            if d.is_cancellation() {
                if found.is_some() {
                    sink.push(
                        rules_of(product),
                        ExchangeRule::CancelWithFile,
                        object,
                        "Cancellations cannot contain data: the exchange set holds the dataset file"
                            .into(),
                    );
                }
                let key = name.to_ascii_uppercase();
                if !cancelled.insert(key.clone()) {
                    sink.push(
                        rules_of(product),
                        ExchangeRule::CancelUnknown,
                        object,
                        "The dataset is cancelled more than once".into(),
                    );
                } else if let Some(system) = on_system {
                    if !system.iter().any(|s| s.eq_ignore_ascii_case(&key)) {
                        sink.push(
                            rules_of(product),
                            ExchangeRule::CancelUnknown,
                            object,
                            "Terminated dataset is not present on the system".into(),
                        );
                    }
                }
                continue;
            }
            check_dataset_metadata(rules_of(product), d, object, &mut sink);
            let Some(i) = found else {
                sink.push(
                    rules_of(product),
                    ExchangeRule::Reference,
                    object,
                    format!(
                        "{rel} is described in the catalogue but not present in the exchange set"
                    ),
                );
                continue;
            };
            let (actual, size) = &inventory.files[i];
            let rules = rules_of(product);
            if *size > rules.max_dataset_bytes() {
                sink.push(rules, ExchangeRule::Size, object, rules.size_message(*size));
            }
            let limits = ReadLimits {
                bytes: MAX_DATASET_BYTES,
                ..ReadLimits::default()
            };
            match read_file(&root.join(actual), None, limits) {
                Ok(ds) => {
                    check_dataset_content(rules_of(product), d, &name, &ds, object, &mut sink);
                    for reference in file_references(&ds) {
                        let target = resolve(&reference);
                        // A file URI, or a bare name resolved like `file::NAME`.
                        let present = target.as_deref().is_some_and(|t| {
                            inventory.find(t).is_some()
                                || inventory
                                    .by_base
                                    .contains_key(&base_name(t).to_ascii_lowercase())
                        });
                        if !present {
                            sink.push(
                                rules_of(product),
                                ExchangeRule::Reference,
                                object,
                                format!(
                                    "File '{reference}' referenced in the dataset is not present in the exchange set"
                                ),
                            );
                        }
                    }
                }
                Err(e) => sink.push(
                    rules_of(product),
                    ExchangeRule::Metadata,
                    object,
                    format!("The dataset cannot be read: {e:#}"),
                ),
            }
        }

        for c in &catalogue.catalogues {
            let rel = resolve(&c.file_name);
            let Some(product) = product_of(c.product_identifier.as_deref(), rel.as_deref()) else {
                continue;
            };
            let cancelled = matches!(c.purpose.as_deref(), Some("cancellation" | "5"));
            match rel.as_deref().map(|r| (r, inventory.find(r))) {
                Some((_, Some(i))) => {
                    listed.insert(i);
                }
                Some((r, None)) if !cancelled => sink.push(
                    rules_of(product),
                    ExchangeRule::Reference,
                    Some(base_name(r)),
                    format!("Catalogue {r} is described in the exchange catalogue but not present"),
                ),
                None => sink.push(
                    rules_of(product),
                    ExchangeRule::Reference,
                    None,
                    format!(
                        "Catalogue fileName '{}' is not a valid file URI",
                        c.file_name
                    ),
                ),
                _ => {}
            }
        }

        for s in &catalogue.support_files {
            let rel = resolve(&s.file_name);
            let Some(product) = product_of(None, rel.as_deref()) else {
                continue;
            };
            let Some(rel) = rel else { continue };
            let object = Some(base_name(&rel));
            let rules = rules_of(product);
            if let Some(message) = rules.support_file_issue(s.resource_purpose.as_deref()) {
                sink.push(rules, ExchangeRule::SupportFile, object, message);
            }
            match inventory.find(&rel) {
                Some(i) => {
                    listed.insert(i);
                }
                None if s.revision_status.as_deref() != Some("deletion") => sink.push(
                    rules_of(product),
                    ExchangeRule::Reference,
                    object,
                    format!(
                        "Support file {rel} is described in the exchange catalogue but not present"
                    ),
                ),
                None => {}
            }
        }
    }

    // Files of S-124 / S-129 product folders that the catalogue does not describe.
    if report.catalogue.is_some() {
        for (i, (rel, _)) in inventory.files.iter().enumerate() {
            let mut parts = rel.split('/');
            let (Some(top), Some(folder)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Some(product) = Product::from_identifier(top).filter(|p| products.contains(p))
            else {
                continue;
            };
            if listed.contains(&i) || base_name(rel).eq_ignore_ascii_case("CATALOG.XML") {
                continue;
            }
            let is_dataset = folder.eq_ignore_ascii_case("DATASET_FILES")
                || base_name(rel).to_ascii_lowercase().ends_with(".gml");
            let (rule, what) = if is_dataset {
                (ExchangeRule::UnlistedDataset, "dataset discovery metadata")
            } else if folder.eq_ignore_ascii_case("CATALOGUES") {
                (
                    ExchangeRule::UnlistedCatalogue,
                    "catalogue discovery metadata",
                )
            } else {
                (
                    ExchangeRule::UnlistedSupport,
                    "support file discovery metadata",
                )
            };
            sink.push(
                rules_of(product),
                rule,
                Some(base_name(rel)),
                format!("{rel} has no {what} in the exchange catalogue"),
            );
        }
    }
    Ok(report)
}

/// Content-table rules of one (non-cancellation) dataset record.
fn check_dataset_metadata(
    rules: &dyn GmlRules,
    d: &DatasetDiscovery,
    object: Option<&str>,
    sink: &mut Sink,
) {
    let product = rules.product();
    for name in rules.mandatory_metadata() {
        if !d.fields.contains(*name) {
            sink.push(
                rules,
                ExchangeRule::MetadataMissing,
                object,
                format!("Mandatory discovery metadata {name} is missing"),
            );
        }
    }
    if let Some(purpose) = d.purpose.as_deref() {
        let allowed = rules.purposes();
        let numeric = rules.numeric_purpose_ok(purpose);
        if !allowed.contains(&purpose) && !numeric {
            sink.push(
                rules,
                ExchangeRule::Purpose,
                object,
                format!(
                    "purpose '{purpose}' is not permitted (allowed: {})",
                    allowed.join(", ")
                ),
            );
        }
    }
    let mut metadata = |message: String| sink.push(rules, ExchangeRule::Metadata, object, message);
    if let Some(p) = d.product_identifier.as_deref() {
        if Product::from_identifier(p) != Some(product) {
            metadata(format!(
                "productIdentifier {p} is not {}",
                product.identifier()
            ));
        }
    }
    if let Some(f) = d.encoding_format.as_deref() {
        if !matches!(f, "GML" | "2") {
            metadata(format!("encodingFormat must be GML, not {f}"));
        }
    }
    if let Some(message) = rules.data_protection_issue(d.data_protection) {
        metadata(message);
    }
    let edition = parse_int(d.edition_number.as_deref());
    let update = parse_int(d.update_number.as_deref());
    match d.purpose.as_deref() {
        Some("newDataset" | "1") if edition.is_some_and(|e| e != 1) => metadata(format!(
            "A new dataset must have edition number 1, not {}",
            edition.unwrap_or_default()
        )),
        Some("newEdition" | "2") if edition.is_some_and(|e| e < 2) => metadata(format!(
            "A new edition must have an edition number above 1, not {}",
            edition.unwrap_or_default()
        )),
        Some("update" | "3") if update.is_none_or(|u| u < 1) => {
            metadata("An update must have an update number of at least 1".into())
        }
        _ => {}
    }
    if matches!(
        d.purpose.as_deref(),
        Some("newDataset" | "newEdition" | "1" | "2")
    ) && update.is_some_and(|u| u != 0)
    {
        metadata("A new dataset or edition must have update number 0".into());
    }
    if let Some(date) = d.issue_date.as_deref() {
        if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err()
            && chrono::NaiveDate::parse_from_str(date, "%Y%m%d").is_err()
        {
            metadata(format!("issueDate '{date}' is not a date"));
        }
    }
    if let Some(b) = d.bounding_box {
        if b[1] > b[3] || b[1] < -90. || b[3] > 90. || b.iter().any(|v| !v.is_finite()) {
            metadata(format!("boundingBox {b:?} is not a valid extent"));
        }
    }
    if let (Some(b), Some(e)) = (d.temporal_begin.as_deref(), d.temporal_end.as_deref()) {
        if let (Some(tb), Some(te)) = (parse_time(b), parse_time(e)) {
            if tb > te {
                metadata("temporalExtent begins after it ends".into());
            }
        }
    }
}

/// S-158-129 check 18 (S-124/exchange/metadata): the record against the content.
fn check_dataset_content(
    rules: &dyn GmlRules,
    d: &DatasetDiscovery,
    name: &str,
    ds: &Dataset,
    object: Option<&str>,
    sink: &mut Sink,
) {
    let product = rules.product();
    let mut metadata = |message: String| sink.push(rules, ExchangeRule::Metadata, object, message);
    if ds.product != product {
        metadata(format!(
            "The catalogue describes a {} dataset; the file is {}",
            product.identifier(),
            ds.product.identifier()
        ));
    }
    let id = &ds.identification;
    if let (Some(m), Some(c)) = (d.product_identifier.as_deref(), id.product_identifier()) {
        if Product::from_identifier(m) != Product::from_identifier(c) {
            metadata(format!(
                "productIdentifier {m} differs from the dataset ({c})"
            ));
        }
    }
    if let (Some(m), Some(c)) = (d.product_version.as_deref(), id.product_edition()) {
        if m.trim() != c.trim() {
            metadata(format!(
                "productSpecification version {m} differs from the dataset productEdition {c}"
            ));
        }
    }
    match id.file_identifier() {
        Some(f) if !f.trim().eq_ignore_ascii_case(name) => metadata(format!(
            "fileName {name} differs from the dataset datasetFileIdentifier {f}"
        )),
        _ => {}
    }
    if let (Some(m), Some(c)) = (
        parse_int(d.update_number.as_deref()),
        parse_int(id.get("updateNumber")),
    ) {
        if m != c {
            metadata(format!(
                "updateNumber {m} differs from the dataset updateNumber {c}"
            ));
        }
    }
    match (d.purpose.as_deref(), id.get("datasetPurpose")) {
        (Some(p @ ("update" | "3")), Some("base")) => metadata(format!(
            "purpose {p} describes an update, but the dataset is a base dataset"
        )),
        (Some(p @ ("newDataset" | "newEdition" | "1" | "2")), Some("update")) => metadata(format!(
            "purpose {p} describes a base dataset, but the dataset is an update"
        )),
        _ => {}
    }
    if let (Some(b), Some(g)) = (d.bounding_box, ds.geometry_bounds()) {
        const EPS: f64 = 1e-7;
        let inside =
            g[0] >= b[0] - EPS && g[1] >= b[1] - EPS && g[2] <= b[2] + EPS && g[3] <= b[3] + EPS;
        // An extent crossing the antimeridian (west > east) is not compared.
        if b[0] <= b[2] && !inside {
            metadata(format!(
                "boundingBox {b:?} does not contain the dataset geometry {g:?}"
            ));
        }
    }
}

#[cfg(test)]
mod tests;
