//! S-124 product rules: dataset validation (clauses 4, 8.1, 9.3, 9.6) and
//! exchange-set rules (clauses 9.5-9.8, 12) on the shared GML validators.
use crate::overlay::S124Overlay;
use ferrite_s100_gml::exchange::ExchangeRule;
use ferrite_s100_gml::product::{GmlOverlayProduct, GmlRules};
use ferrite_s100_gml::validate::{
    file_name_follows, parse_date_time, upper_alnum, FcRule, Incoming, IssueSink, Validator,
};
use ferrite_s100_gml::{Dataset, Product, Severity};
use std::collections::HashSet;

/// S-124 9.6: datasets must not exceed 50 KB.
pub const MAX_DATASET_BYTES: u64 = 50 * 1024;

/// Non-S-100 sources the S-124 module imports (SHOM PING KML, GPX and
/// shapefiles; ZIP archives of them when selected explicitly).
pub const IMPORT_EXTENSIONS: [&str; 3] = ["kml", "gpx", "shp"];

fn size_message(size: u64) -> String {
    format!("Dataset is {size} bytes; S-124 datasets must not exceed 50 KB")
}

/// The S-124 rules (a `static` registered with the engine).
pub struct S124Rules;

/// The S-124 product module.
pub static S124: S124Rules = S124Rules;

impl GmlRules for S124Rules {
    fn product(&self) -> Product {
        Product::S124
    }

    fn fc_rule_check(&self, rule: FcRule) -> (String, Severity) {
        (format!("FC/{}", rule.name()), rule.severity())
    }

    /// S-100 17-4.3 naming: `124`, producer, upper-case alphanumerics.
    fn file_name_ok(&self, name: &str) -> bool {
        file_name_follows(Product::S124, name, |unique| {
            unique.chars().all(upper_alnum)
        })
    }

    fn checked_by_product(&self, class: &str, role: &str, _target: &str) -> bool {
        (class, role) == ("NavwarnPart", "header") // S-124/header
    }

    fn product_checks(
        &self,
        _validator: &Validator,
        dataset: &Dataset,
        byte_size: Option<u64>,
        _incoming: &Incoming,
        sink: &mut IssueSink,
    ) {
        s124_checks(dataset, byte_size, sink);
    }

    fn exchange_check(&self, rule: ExchangeRule) -> (String, Severity) {
        let name = match rule {
            ExchangeRule::Catalogue | ExchangeRule::CatalogueName => "catalogue",
            ExchangeRule::Signature => "signature",
            ExchangeRule::FileName => "file-name",
            ExchangeRule::Reference => "reference",
            ExchangeRule::Metadata | ExchangeRule::MetadataMissing => "metadata",
            ExchangeRule::UnlistedDataset => "dataset-metadata",
            ExchangeRule::UnlistedCatalogue => "catalogue-metadata",
            ExchangeRule::UnlistedSupport | ExchangeRule::SupportFile => "support-file",
            ExchangeRule::Purpose => "purpose",
            ExchangeRule::Size => "size",
            ExchangeRule::CancelUnknown => "cancellation-unknown",
            ExchangeRule::CancelWithFile => "cancellation-file",
        };
        let severity = match rule {
            ExchangeRule::MetadataMissing | ExchangeRule::CancelUnknown => Severity::Warning,
            _ => Severity::Error,
        };
        (format!("S-124/exchange/{name}"), severity)
    }

    /// S-124 12.2.2.
    fn mandatory_metadata(&self) -> &'static [&'static str] {
        &[
            "compressionFlag",
            "dataProtection",
            "digitalSignatureReference",
            "digitalSignatureValue",
            "copyright",
            "notForNavigation",
            "issueDate",
            "boundingBox",
            "productSpecification",
            "producingAgency",
            "encodingFormat",
        ]
    }

    fn purposes(&self) -> &'static [&'static str] {
        &["newDataset", "cancellation"]
    }

    fn numeric_purpose_ok(&self, purpose: &str) -> bool {
        matches!(purpose, "1" | "5")
    }

    fn max_dataset_bytes(&self) -> u64 {
        MAX_DATASET_BYTES
    }

    fn size_message(&self, size: u64) -> String {
        size_message(size)
    }

    /// S-124 9.5: only language packs are support files.
    fn support_file_issue(&self, resource_purpose: Option<&str>) -> Option<String> {
        (resource_purpose != Some("languagePack")).then(|| {
            format!(
                "S-124 exchange sets may only include language packs as support files (resourcePurpose {})",
                resource_purpose.unwrap_or("missing")
            )
        })
    }

    fn data_protection_issue(&self, protected: Option<bool>) -> Option<String> {
        (protected == Some(true)).then(|| "dataProtection must be false for S-124".into())
    }

    fn import_extensions(&self) -> &'static [&'static str] {
        &IMPORT_EXTENSIONS
    }

    fn overlay(&self) -> Box<dyn GmlOverlayProduct> {
        Box::new(S124Overlay::default())
    }
}

fn s124_checks(ds: &Dataset, byte_size: Option<u64>, sink: &mut IssueSink) {
    let err = Severity::Error;
    if let Some(size) = byte_size {
        if size > MAX_DATASET_BYTES {
            sink.product("S-124/size", err, None, size_message(size));
        }
    }
    let preambles: Vec<_> = ds.objects_of("NavwarnPreamble").map(|(i, _)| i).collect();
    if preambles.len() != 1 {
        sink.product(
            "S-124/preamble",
            err,
            None,
            format!(
                "Dataset has {} NavwarnPreamble instances; exactly one is required",
                preambles.len()
            ),
        );
    }
    let parts: Vec<_> = ds.objects_of("NavwarnPart").collect();
    for (_, part) in &parts {
        let headers: HashSet<usize> = part
            .references
            .iter()
            .filter(|r| r.role == "header")
            .filter_map(|r| r.target)
            .filter(|t| ds.objects[*t].code == "NavwarnPreamble")
            .collect();
        if headers.len() != 1
            || part
                .references
                .iter()
                .filter(|r| r.role == "header")
                .count()
                != 1
        {
            sink.product(
                "S-124/header",
                err,
                Some(&part.id),
                "NavwarnPart must have exactly one header reference to the NavwarnPreamble".into(),
            );
        }
        for wi in part.attributes_named("warningInformation") {
            let has = |c: &str| wi.children().iter().any(|a| a.code == c);
            if !has("information") && !has("navwarnTypeDetails") {
                sink.product(
                    "S-124/warning-information",
                    err,
                    Some(&part.id),
                    "warningInformation must contain information or navwarnTypeDetails".into(),
                );
            }
            let mut languages = HashSet::new();
            for info in wi.children().iter().filter(|a| a.code == "information") {
                let lang = info
                    .children()
                    .iter()
                    .find(|c| c.code == "language")
                    .and_then(|c| c.text());
                if !languages.insert(lang.unwrap_or("").trim().to_string()) {
                    sink.product(
                        "S-124/warning-information",
                        err,
                        Some(&part.id),
                        format!(
                            "warningInformation has more than one information in language {lang:?}"
                        ),
                    );
                }
            }
        }
        for range in part.attributes_named("fixedDateRange") {
            let has = |c: &str| range.children().iter().any(|a| a.code == c);
            for (time, date) in [("timeOfDayStart", "dateStart"), ("timeOfDayEnd", "dateEnd")] {
                if has(time) && !has(date) {
                    sink.product(
                        "S-124/fixed-date-range",
                        err,
                        Some(&part.id),
                        format!("fixedDateRange: {time} requires {date}"),
                    );
                }
            }
        }
    }
    for &p in &preambles {
        let pre = &ds.objects[p];
        // Associated with at least one References or NavwarnPart (either direction).
        let linked = pre
            .references
            .iter()
            .filter_map(|r| r.target)
            .chain(ds.objects.iter().enumerate().filter_map(|(j, o)| {
                o.references
                    .iter()
                    .any(|r| r.target == Some(p))
                    .then_some(j)
            }))
            .any(|t| matches!(ds.objects[t].code.as_str(), "References" | "NavwarnPart"));
        if !linked {
            sink.product(
                "S-124/preamble-content",
                err,
                Some(&pre.id),
                "NavwarnPreamble must be associated with at least one References or NavwarnPart"
                    .into(),
            );
        }
        if let (Some(c), Some(p)) = (
            pre.value(&["cancellationDate"]),
            pre.value(&["publicationTime"]),
        ) {
            if let (Some(c), Some(p)) = (parse_date_time(c), parse_date_time(p)) {
                if c <= p {
                    sink.product(
                        "S-124/cancellation-date",
                        Severity::Warning,
                        Some(&pre.id),
                        "cancellationDate is not later than publicationTime".into(),
                    );
                }
            }
        }
    }

    // References content (S-124 4.x constraints, 8.1.2, 8.1.4, 9.3).
    let references: Vec<_> = ds.objects_of("References").collect();
    let mut in_force = Vec::new();
    let mut cancellations = 0;
    for (i, r) in &references {
        let id = Some(r.id.as_str());
        let no_message = r
            .value(&["noMessageOnHand"])
            .map(|v| matches!(v.trim(), "true" | "1"));
        let category = r.value(&["referenceCategory"]).map(str::trim);
        let series = r.attributes_named("messageSeriesIdentifier").count();
        match no_message {
            Some(true) if series > 0 => sink.product(
                "S-124/references",
                err,
                id,
                "messageSeriesIdentifier is prohibited when noMessageOnHand is true".into(),
            ),
            Some(false) if series == 0 => sink.product(
                "S-124/references",
                err,
                id,
                "messageSeriesIdentifier is mandatory when noMessageOnHand is false".into(),
            ),
            _ => {}
        }
        if no_message == Some(true) && category != Some("3") {
            sink.product(
                "S-124/references",
                Severity::Warning,
                id,
                "noMessageOnHand=true is only meaningful in an in-force bulletin (referenceCategory 3)".into(),
            );
        }
        match category {
            Some("3") => in_force.push((*i, no_message == Some(true))),
            Some("1") => cancellations += 1,
            _ => {}
        }
    }
    if !in_force.is_empty() {
        if in_force.len() > 1 {
            sink.product(
                "S-124/in-force-bulletin",
                err,
                None,
                format!("In-force bulletin has {} References with referenceCategory 3; exactly one is required", in_force.len()),
            );
        }
        for o in &ds.objects {
            if matches!(o.code.as_str(), "NavwarnAreaAffected" | "TextPlacement")
                || !o.geometry.is_empty()
            {
                sink.product(
                    "S-124/in-force-bulletin",
                    err,
                    Some(&o.id),
                    format!("In-force bulletin must not contain geometry, NavwarnAreaAffected or TextPlacement ({})", o.code),
                );
            }
        }
        if let Some((keep, _)) = in_force.iter().find(|(_, nmoh)| *nmoh) {
            for (j, o) in ds.objects.iter().enumerate() {
                if j != *keep && o.code != "NavwarnPreamble" {
                    sink.product(
                        "S-124/in-force-bulletin",
                        err,
                        Some(&o.id),
                        format!("In-force bulletin with noMessageOnHand=true permits no other content ({})", o.code),
                    );
                }
            }
        }
    } else if parts.is_empty() && cancellations > 1 {
        sink.product(
            "S-124/cancellation",
            Severity::Warning,
            None,
            format!("Cancellation dataset should contain only one References instance, found {cancellations}"),
        );
    }
    for code in ["NavwarnAreaAffected", "TextPlacement"] {
        for (_, o) in ds.objects_of(code) {
            if o.geometry.is_empty() {
                sink.product(
                    "S-124/geometry",
                    err,
                    Some(&o.id),
                    format!("{code} must have geometry"),
                );
            }
        }
    }
}
