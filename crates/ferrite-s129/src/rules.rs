//! S-129 product rules: dataset validation (S-158-129, every check a
//! warning per A.1.3, with its number where one applies; S-129 18.2) and
//! exchange-set rules (S-158-129 checks 11, 17-20; S-129 19.4).
use crate::overlay::S129Overlay;
use ferrite_s100_gml::exchange::{ExchangeRule, PURPOSES};
use ferrite_s100_gml::product::{GmlOverlayProduct, GmlRules};
use ferrite_s100_gml::validate::{
    file_name_follows, parse_date_time, upper_alnum, AssociationBinding, FcRule, Incoming,
    IssueSink, Validator,
};
use ferrite_s100_gml::{Dataset, GmlIdentity, Severity};

/// S-129 18.2.2: recommended maximum dataset size.
pub const MAX_DATASET_BYTES: u64 = 20 * 1024 * 1024;

fn size_message(size: u64) -> String {
    format!("Dataset is {size} bytes; the recommended maximum is 20 MB")
}

/// S-158-129 check number of a generic feature-catalogue rule.
pub fn s158_129(rule: FcRule) -> Option<u32> {
    Some(match rule {
        FcRule::MandatoryMissing => 1,
        FcRule::MandatoryNil => 2,
        FcRule::Zeros => 3,
        FcRule::RoleNotPermitted => 5,
        FcRule::RoleUnknown => 7,
        FcRule::WrongTarget => 8,
        FcRule::WrongAssociation => 4,
        FcRule::UnknownAssociation => 6,
        FcRule::RoleOnWrongAssociation => 9,
        FcRule::ValueFormat | FcRule::Enumeration => 10,
        FcRule::UnknownClass => 12,
        FcRule::UnknownAttribute => 13,
        FcRule::AttributeNotPermitted => 14,
        FcRule::Order => 15,
        FcRule::TooMany => 16,
        FcRule::OptionalEmpty => 21,
        FcRule::FileName => 22,
        FcRule::DateTimeFormat => 26,
        FcRule::Primitive | FcRule::AssociationMultiplicity | FcRule::Product => return None,
    })
}

/// How S-129 datasets name their product; S-158-129 check 15 reports a
/// DatasetIdentificationInformation that follows the members.
pub static IDENTITY: GmlIdentity = GmlIdentity {
    id: crate::PRODUCT,
    identifiers: &["S-129", "S129"],
    code: "129",
    late_identification_check: "S-158-129/15",
};

/// The S-129 rules (a `static` registered with the engine).
pub struct S129Rules;

/// The S-129 product module.
pub static S129: S129Rules = S129Rules;

impl GmlRules for S129Rules {
    fn identity(&self) -> &'static GmlIdentity {
        &IDENTITY
    }

    fn fc_rule_check(&self, rule: FcRule) -> (String, Severity) {
        match s158_129(rule) {
            Some(n) => (format!("S-158-129/{n}"), Severity::Warning),
            None => (format!("FC/{}", rule.name()), Severity::Warning),
        }
    }

    /// S-129 18.2.3: `129`, producer, at most 8 upper-case alphanumerics or
    /// underscores.
    fn file_name_ok(&self, name: &str) -> bool {
        file_name_follows(&IDENTITY, name, |unique| {
            unique.len() <= 8 && unique.chars().all(|c| upper_alnum(c) || c == '_')
        })
    }

    /// The plan's control point composition (S-158-129/27).
    fn checked_by_product(&self, class: &str, role: &str, target: &str) -> bool {
        (class, role, target)
            == (
                "UnderKeelClearancePlan",
                "theComponent",
                "UnderKeelClearanceControlPoint",
            )
    }

    fn dataset_checks(&self, _dataset: &Dataset, byte_size: Option<u64>, sink: &mut IssueSink) {
        if let Some(size) = byte_size {
            if size > MAX_DATASET_BYTES {
                sink.product("S-129/size", Severity::Warning, None, size_message(size));
            }
        }
    }

    fn product_checks(
        &self,
        validator: &Validator,
        dataset: &Dataset,
        _byte_size: Option<u64>,
        incoming: &Incoming,
        sink: &mut IssueSink,
    ) {
        s129_checks(validator, dataset, incoming, sink);
    }

    fn exchange_check(&self, rule: ExchangeRule) -> (String, Severity) {
        let id = match rule {
            ExchangeRule::CatalogueName | ExchangeRule::FileName => "S-158-129/11",
            ExchangeRule::Reference => "S-158-129/17",
            ExchangeRule::Metadata
            | ExchangeRule::MetadataMissing
            | ExchangeRule::UnlistedDataset
            | ExchangeRule::Purpose => "S-158-129/18",
            ExchangeRule::CancelUnknown => "S-158-129/19",
            ExchangeRule::CancelWithFile => "S-158-129/20",
            ExchangeRule::Catalogue => "S-129/exchange/catalogue",
            ExchangeRule::Signature => "S-129/exchange/signature",
            ExchangeRule::UnlistedCatalogue
            | ExchangeRule::UnlistedSupport
            | ExchangeRule::SupportFile => "S-129/exchange/unlisted",
            ExchangeRule::Size => "S-129/exchange/size",
        };
        (id.to_string(), Severity::Warning)
    }

    /// S-129 19.4.
    fn mandatory_metadata(&self) -> &'static [&'static str] {
        &[
            "compressionFlag",
            "dataProtection",
            "digitalSignatureReference",
            "digitalSignatureValue",
            "copyright",
            "notForNavigation",
            "issueDate",
            "purpose",
            "specificUsage",
            "editionNumber",
            "issueTime",
            "temporalExtent",
            "productSpecification",
            "producingAgency",
            "encodingFormat",
            "dataCoverage",
        ]
    }

    fn purposes(&self) -> &'static [&'static str] {
        &PURPOSES
    }

    fn numeric_purpose_ok(&self, purpose: &str) -> bool {
        matches!(purpose, "1" | "2" | "3" | "4" | "5" | "6")
    }

    fn max_dataset_bytes(&self) -> u64 {
        MAX_DATASET_BYTES
    }

    fn size_message(&self, size: u64) -> String {
        size_message(size)
    }

    fn overlay(&self) -> Box<dyn GmlOverlayProduct> {
        Box::new(S129Overlay::default())
    }
}

fn s129_checks(validator: &Validator, ds: &Dataset, incoming: &Incoming, sink: &mut IssueSink) {
    for o in &ds.objects {
        let id = Some(o.id.as_str());
        for r in o.attributes_named("fixedTimeRange") {
            let value = |name: &str| {
                r.children()
                    .iter()
                    .find(|c| c.code == name)
                    .and_then(|c| c.text())
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
            };
            match (value("timeStart"), value("timeEnd")) {
                (Some(s), Some(e)) => {
                    if let (Some(s), Some(e)) = (parse_date_time(s), parse_date_time(e)) {
                        if s > e {
                            sink.product(
                                "S-158-129/23",
                                Severity::Warning,
                                id,
                                "fixedTimeRange: timeStart is later than timeEnd".into(),
                            );
                        }
                    }
                }
                (Some(_), None) => sink.product(
                    "S-158-129/24",
                    Severity::Warning,
                    id,
                    "fixedTimeRange: timeStart without timeEnd".into(),
                ),
                (None, Some(_)) => sink.product(
                    "S-158-129/25",
                    Severity::Warning,
                    id,
                    "fixedTimeRange: timeEnd without timeStart".into(),
                ),
                (None, None) => {}
            }
        }
    }
    let binding = AssociationBinding {
        role: "theComponent".into(),
        association: Some("UnderKeelClearanceControlPointComposition".into()),
        target: "UnderKeelClearanceControlPoint".into(),
        lower: 1,
        upper: None,
    };
    let mut binding = binding;
    if let Some(b) = validator
        .association_bindings("UnderKeelClearancePlan")
        .into_iter()
        .find(|b| b.target == "UnderKeelClearanceControlPoint")
    {
        binding = b;
    }
    for (i, plan) in ds.objects_of("UnderKeelClearancePlan") {
        if validator
            .associated(ds, i, plan, &binding, incoming)
            .is_empty()
        {
            sink.product(
                "S-158-129/27",
                Severity::Warning,
                Some(&plan.id),
                "An UnderKeelClearancePlan must consist of at least 1 UnderKeelClearanceControlPoint association".into(),
            );
        }
    }
}
