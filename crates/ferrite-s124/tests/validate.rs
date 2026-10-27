//! S-124 dataset validation rules on the shared feature-catalogue validator.
mod common;

use common::*;
use ferrite_s100_gml::product::GmlRules;
use ferrite_s100_gml::validate::{validate, Validator};
use ferrite_s100_gml::{read_bytes, Dataset, ReadLimits, Severity};
use ferrite_s124::S124;

#[test]
fn file_names() {
    assert!(S124.file_name_ok("124GB00S1Y2026N36.GML"));
    assert!(!S124.file_name_ok("124GB0.GML"));
}

#[test]
fn s124_ukho_datasets() {
    let Some(fc) = fc(S124_FC) else { return };
    let v = Validator::new(&fc);
    let mut all = Vec::new();
    for name in [
        "124GB00S1Y2026N36.GML",
        "124GB00S2Y2026N43.GML",
        "124GB00S2Y2026N44.GML",
        "124GB00S2Y2026N45.GML",
        "124GB00S2Y2026N46.GML",
    ] {
        let Some(ds) = read(&format!("{S124_DIR}/{name}"), &fc) else {
            return;
        };
        let issues = validate(&ds, &fc, &S124);
        assert_eq!(
            issues,
            v.validate(
                &ds,
                std::fs::metadata(ds.source.as_ref().unwrap())
                    .ok()
                    .map(|m| m.len()),
                &S124
            )
        );
        let per_file = checks(&issues);
        // Every file uses countryName (FC: nationality), so nationality is
        // also reported missing only if mandatory (it is optional).
        assert!(
            count(&issues, "FC/unknown-attribute") >= 1,
            "{name}: {per_file:?}"
        );
        assert!(
            !per_file.contains_key("S-124/header"),
            "{name}: {per_file:?}"
        );
        assert!(!per_file.contains_key("S-124/preamble"), "{name}");
        assert!(!per_file.contains_key("S-124/preamble-content"), "{name}");
        assert!(!per_file.contains_key("S-124/size"), "{name}");
        assert!(!per_file.contains_key("FC/file-name"), "{name}");
        assert!(
            !per_file.contains_key("FC/association-multiplicity"),
            "{name}: {issues:#?}"
        );
        all.extend(issues);
    }
    let totals = checks(&all);
    // countryName in all 5 files; featureName/Name in N43, N44, N45.
    assert_eq!(totals.get("FC/unknown-attribute"), Some(&8), "{totals:?}");
    let names: Vec<_> = all
        .iter()
        .filter(|i| i.check == "FC/unknown-attribute")
        .map(|i| i.message.split(':').next().unwrap())
        .collect();
    assert_eq!(
        names.iter().filter(|n| n.ends_with(".countryName")).count(),
        5
    );
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == "NavwarnPart.featureName.Name")
            .count(),
        3
    );
    // ... which leaves featureName without its mandatory name.
    assert_eq!(totals.get("FC/mandatory-missing"), Some(&3), "{totals:?}");
    assert!(all
        .iter()
        .filter(|i| i.check == "FC/mandatory-missing")
        .all(|i| i.message.starts_with("NavwarnPart.featureName.name")));
    assert_eq!(totals.len(), 2, "{totals:?}");
    assert!(all.iter().all(|i| i.severity == Severity::Error));
}

#[test]
fn s124_association_inference() {
    let Some(fc) = fc(S124_FC) else { return };
    let ds = s124(&format!(
        r##"{PREAMBLE}</S124:NavwarnPreamble>
<S124:References gml:id="R"><S124:referenceCategory code="1"/><S124:theWarning xlink:href="#PRE"/><S124:affects xlink:href="#P"/></S124:References>
<S124:NavwarnPart gml:id="P"><S124:header xlink:href="#PRE"/><S124:affects xlink:href="#A"/><S124:theWarning xlink:href="#A"/><S124:header xlink:href="#A"/><S124:impacts xlink:href="#A"/></S124:NavwarnPart>
<S124:NavwarnAreaAffected gml:id="A"><S124:impacts xlink:href="#P"/></S124:NavwarnAreaAffected>"##
    ));
    let issues = Validator::new(&fc).validate(&ds, None, &S124);
    let found: Vec<_> = issues
        .iter()
        .filter(|i| {
            i.check.starts_with("FC/association-") && i.check != "FC/association-multiplicity"
                || i.check.starts_with("FC/role-")
        })
        .map(|i| (i.check.as_str(), i.object.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(
        found,
        [
            ("FC/association-unknown", "R"), // affects: References - NavwarnPart
            ("FC/association-wrong", "P"),   // theWarning on Part - AreaAffected
            ("FC/role-association", "P"),    // header (Part's own) on areaAffected
            ("FC/role-not-permitted", "P"),  // impacts: the area's end
        ],
        "{issues:#?}"
    );
    assert!(issues
        .iter()
        .filter(|i| i.check.starts_with("FC/association-u")
            || i.check.starts_with("FC/association-w")
            || i.check == "FC/role-association")
        .all(|i| i.severity == Severity::Error));
}

fn s124(members: &str) -> Dataset {
    let xml = format!(
        r#"<S124:Dataset xmlns:S124="http://www.iho.int/S124/gml/2.0" xmlns:S100="http://www.iho.int/s100gml/5.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:xlink="http://www.w3.org/1999/xlink" gml:id="DS">
<S100:DatasetIdentificationInformation><S100:productIdentifier>S-124</S100:productIdentifier><S100:productEdition>2.0.0</S100:productEdition><S100:datasetFileIdentifier>124GB00IFB1.GML</S100:datasetFileIdentifier></S100:DatasetIdentificationInformation>
<S124:members>{members}</S124:members></S124:Dataset>"#
    );
    read_bytes(xml.as_bytes(), None, ReadLimits::default()).unwrap()
}

const PREAMBLE: &str = r#"<S124:NavwarnPreamble gml:id="PRE"><S124:generalArea><S124:locationName><S124:language>eng</S124:language><S124:text>WIGHT</S124:text></S124:locationName></S124:generalArea><S124:messageSeriesIdentifier><S124:agencyResponsibleForProduction>UKHO</S124:agencyResponsibleForProduction><S124:nameOfSeries>WZ</S124:nameOfSeries><S124:warningNumber>1</S124:warningNumber><S124:warningType code="2"/><S124:year>2026</S124:year></S124:messageSeriesIdentifier><S124:cancellationDate>2026-01-01T00:00:00Z</S124:cancellationDate><S124:intService>true</S124:intService><S124:navwarnTypeGeneral code="8"/><S124:publicationTime>2026-02-13T08:42:41Z</S124:publicationTime>"#;

#[test]
fn s124_in_force_bulletin_rules() {
    let Some(fc) = fc(S124_FC) else { return };
    let v = Validator::new(&fc);
    // No message on hand: only the preamble and one References.
    let ok = s124(&format!(
        r##"{PREAMBLE}<S124:theReferences xlink:href="#R"/></S124:NavwarnPreamble>
<S124:References gml:id="R"><S124:noMessageOnHand>true</S124:noMessageOnHand><S124:referenceCategory code="3"/></S124:References>"##
    ));
    let issues = v.validate(&ok, Some(1000), &S124);
    assert_eq!(
        checks(&issues).into_iter().collect::<Vec<_>>(),
        [("S-124/cancellation-date", 1)],
        "{issues:#?}"
    );

    let bad = s124(&format!(
        r##"{PREAMBLE}</S124:NavwarnPreamble>
<S124:References gml:id="R"><S124:messageSeriesIdentifier><S124:agencyResponsibleForProduction>UKHO</S124:agencyResponsibleForProduction><S124:nameOfSeries>WZ</S124:nameOfSeries><S124:warningNumber>1</S124:warningNumber><S124:warningType code="2"/><S124:year>2026</S124:year></S124:messageSeriesIdentifier><S124:noMessageOnHand>true</S124:noMessageOnHand><S124:referenceCategory code="3"/><S124:theWarning xlink:href="#PRE"/></S124:References>
<S124:NavwarnPart gml:id="P"><S124:fixedDateRange><S124:timeOfDayStart>08:00:00</S124:timeOfDayStart></S124:fixedDateRange><S124:warningInformation/><S124:header xlink:href="#PRE"/><S124:header xlink:href="#PRE"/><S124:geometry><S100:pointProperty><S100:Point gml:id="G"><gml:pos>50 -1</gml:pos></S100:Point></S100:pointProperty></S124:geometry></S124:NavwarnPart>
<S124:NavwarnAreaAffected gml:id="A"><S124:impacts xlink:href="#P"/></S124:NavwarnAreaAffected>"##
    ));
    let issues = v.validate(&bad, Some(60_000), &S124);
    let c = checks(&issues);
    assert_eq!(c.get("S-124/size"), Some(&1));
    assert_eq!(c.get("S-124/references"), Some(&1), "{issues:#?}");
    assert_eq!(c.get("S-124/header"), Some(&1));
    assert_eq!(c.get("S-124/warning-information"), Some(&1));
    assert_eq!(c.get("S-124/fixed-date-range"), Some(&1));
    assert_eq!(c.get("S-124/geometry"), Some(&1));
    // P has geometry, A is an area; with noMessageOnHand both are extra content.
    assert_eq!(c.get("S-124/in-force-bulletin"), Some(&4), "{issues:#?}");
    // warningInformation is mandatory and present but empty.
    assert_eq!(c.get("FC/mandatory-nil"), Some(&1), "{issues:#?}");
}

#[test]
fn s124_preamble_rules() {
    let Some(fc) = fc(S124_FC) else { return };
    let ds = s124(&format!(
        "{PREAMBLE}</S124:NavwarnPreamble>{}",
        PREAMBLE.replace("\"PRE\"", "\"PRE2\"") + "</S124:NavwarnPreamble>"
    ));
    let issues = Validator::new(&fc).validate(&ds, None, &S124);
    let c = checks(&issues);
    assert_eq!(c.get("S-124/preamble"), Some(&1));
    assert_eq!(c.get("S-124/preamble-content"), Some(&2), "{issues:#?}");
    assert!(workspace_file(S124_FC).is_some());
}
