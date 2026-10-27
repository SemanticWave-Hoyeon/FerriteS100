//! S-129 dataset validation rules on the shared feature-catalogue validator.
mod common;

use common::*;
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_gml::product::GmlRules;
use ferrite_s100_gml::validate::{validate, Validator};
use ferrite_s100_gml::{read_bytes, Dataset, ReadLimits, Severity};
use ferrite_s129::S129;

#[test]
fn file_names() {
    assert!(S129.file_name_ok("12900MCTDS130TS.gml"));
    assert!(S129.file_name_ok("129ABCD.GML"));
    assert!(S129.file_name_ok("129ABCD_1234567.GML"));
    assert!(!S129.file_name_ok("129ABCD123456789.GML"));
    assert!(!S129.file_name_ok("129abcd.GML"));
    assert!(!S129.file_name_ok("129ABCD.xml"));
    assert!(!S129.file_name_ok("124ABCD.GML"));
}

#[test]
fn s129_iho_datasets() {
    let Some(fc) = fc(S129_FC) else { return };
    let Some(a) = read(S129_130, &fc) else { return };
    let Some(b) = read(S129_200, &fc) else { return };
    for (ds, empty_ranges) in [(&a, 7), (&b, 0)] {
        let issues = validate(ds, &fc, &S129);
        assert!(issues.iter().all(|i| i.severity == Severity::Warning));
        let c = checks(&issues);
        // The test datasets encode no associations at all.
        assert_eq!(c.get("S-158-129/27"), Some(&1), "{c:?}");
        // Every area and control point lacks its theCollection link to the plan.
        assert_eq!(
            c.get("FC/association-multiplicity"),
            Some(&(1 + 87 + 200 + 15)),
            "{c:?}"
        );
        // 130TS has seven empty <fixedTimeRange/> elements.
        assert_eq!(
            c.get("S-158-129/21").copied().unwrap_or(0),
            empty_ranges,
            "{c:?}"
        );
        for n in [1, 2, 3, 5, 7, 8, 12, 13, 14, 15, 16, 22, 23, 24, 25, 26] {
            assert_eq!(count(&issues, &format!("S-158-129/{n}")), 0, "{n}: {c:?}");
        }
        assert_eq!(issues.len(), 1 + 303 + empty_ranges, "{c:?}");
    }
}

fn s129(members: &str) -> Dataset {
    let xml = format!(
        r#"<Dataset xmlns="http://www.iho.int/S129/2.0" xmlns:S100="http://www.iho.int/s100gml/5.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" gml:id="DS">
<S100:DatasetIdentificationInformation><S100:productIdentifier>S-129</S100:productIdentifier><S100:productEdition>2.0.0</S100:productEdition><S100:datasetFileIdentifier>129ABCD01.GML</S100:datasetFileIdentifier></S100:DatasetIdentificationInformation>
<members>{members}</members></Dataset>"#
    );
    read_bytes(xml.as_bytes(), None, ReadLimits::default()).unwrap()
}

#[test]
fn s129_synthetic_violations() {
    let Some(fc) = fc(S129_FC) else { return };
    let ds = s129(
        r##"<UnderKeelClearancePlan gml:id="PLAN">
  <generationTime>2024-04-15 23:11</generationTime>
  <fixedTimeRange><timeStart>2024-04-18T01:00:00Z</timeStart><timeEnd>2024-04-17T01:00:00Z</timeEnd></fixedTimeRange>
  <vesselID>1</vesselID><vesselID>2</vesselID>
  <maximumDraught>012.20</maximumDraught>
  <underKeelClearancePurpose code="9">Bogus</underKeelClearancePurpose>
  <scaleMinimum>1</scaleMinimum>
  <madeUp>1</madeUp>
  <theComponent xlink:href="#CP"/>
</UnderKeelClearancePlan>
<UnderKeelClearanceControlPoint gml:id="CP">
  <featureName><language>eng</language><nameUsage code="3"/></featureName>
  <expectedPassingSpeed xsi:nil="true" nilReason="unknown"/>
  <fixedTimeRange><timeEnd>2024-04-17T01:00:00Z</timeEnd></fixedTimeRange>
  <theCollection xlink:href="#PLAN"/><theNothing xlink:href="#PLAN"/><theComponent xlink:href="#PLAN"/>
</UnderKeelClearanceControlPoint>
<UnderKeelClearanceControlPoint gml:id="CP2"><fixedTimeRange><timeStart>2024-04-17T01:00:00Z</timeStart></fixedTimeRange><theCollection xlink:href="#CP"/></UnderKeelClearanceControlPoint>
<Bogus gml:id="X"/>"##,
    );
    let issues = Validator::new(&fc).validate(&ds, None, &S129);
    let c = checks(&issues);
    let expect = [
        ("S-158-129/1", 1),  // featureName.name missing
        ("S-158-129/3", 1),  // 012.20
        ("S-158-129/5", 1),  // theComponent on a control point
        ("S-158-129/7", 1),  // theNothing
        ("S-158-129/8", 1),  // CP2 theCollection -> control point
        ("S-158-129/10", 2), // enumeration codes 9 and 3
        ("S-158-129/12", 1), // Bogus
        ("S-158-129/13", 1), // madeUp
        ("S-158-129/14", 1), // scaleMinimum on the plan
        ("S-158-129/15", 1), // generationTime before fixedTimeRange
        ("S-158-129/16", 1), // two vesselID
        ("S-158-129/21", 1), // nil expectedPassingSpeed
        ("S-158-129/23", 1),
        ("S-158-129/24", 1),
        ("S-158-129/25", 1),
        ("S-158-129/26", 1),
        ("FC/association-multiplicity", 1), // CP2 lacks a plan
    ];
    for (check, n) in expect {
        assert_eq!(
            c.get(check).copied().unwrap_or(0),
            n,
            "{check}: {issues:#?}"
        );
    }
    assert_eq!(
        issues.len(),
        expect.iter().map(|e| e.1).sum::<usize>(),
        "{c:?}"
    );
    // The plan's control point arrives through the inverse theCollection role.
    assert_eq!(count(&issues, "S-158-129/27"), 0);
}

/// The S-129 catalogue with an orphan role and a second association
/// (ControlPointMonitoring: theMonitor/theMonitored, bound on control
/// points towards plan areas), so that checks 4, 6 and 9 are reachable.
fn patched_s129_fc(tag: &str) -> Option<FeatureCatalogue> {
    let text = std::fs::read_to_string(workspace_file(S129_FC)?).unwrap();
    let role = |code: &str| {
        format!("<S100FC:S100_FC_Role><S100FC:name>{code}</S100FC:name><S100FC:definition>Test role.</S100FC:definition><S100FC:code>{code}</S100FC:code></S100FC:S100_FC_Role>")
    };
    let mut patched = text.replacen(
        "</S100FC:S100_FC_Roles>",
        &format!(
            "{}{}{}</S100FC:S100_FC_Roles>",
            role("theMonitor"),
            role("theMonitored"),
            role("theOrphan")
        ),
        1,
    );
    patched = patched.replacen(
        "</S100FC:S100_FC_FeatureAssociations>",
        r#"<S100FC:S100_FC_FeatureAssociation isAbstract="false"><S100FC:name>Monitoring</S100FC:name><S100FC:definition>Test association.</S100FC:definition><S100FC:code>ControlPointMonitoring</S100FC:code><S100FC:role ref="theMonitor"/><S100FC:role ref="theMonitored"/></S100FC:S100_FC_FeatureAssociation></S100FC:S100_FC_FeatureAssociations>"#,
        1,
    );
    // The last binding in the catalogue is the control point's plan link.
    let at = patched.rfind("</S100FC:featureBinding>").unwrap() + "</S100FC:featureBinding>".len();
    assert!(patched[..at]
        .trim_end_matches("</S100FC:featureBinding>")
        .trim_end()
        .ends_with("<S100FC:featureType ref=\"UnderKeelClearancePlan\"/>"));
    patched.insert_str(
        at,
        r#"<S100FC:featureBinding roleType="association"><S100FC:multiplicity><S100Base:lower>0</S100Base:lower><S100Base:upper xsi:nil="true" infinite="true"/></S100FC:multiplicity><S100FC:association ref="ControlPointMonitoring"/><S100FC:role ref="theMonitored"/><S100FC:featureType ref="UnderKeelClearancePlanArea"/></S100FC:featureBinding>"#,
    );
    let dir = std::env::temp_dir().join(format!("ferrite-gml-fc-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("129_FC_patched.xml");
    std::fs::write(&path, patched).unwrap();
    Some(FeatureCatalogue::load(&path).unwrap())
}

#[test]
fn s129_association_inference() {
    let Some(fc) = patched_s129_fc("assoc") else {
        return;
    };
    let v = Validator::new(&fc);
    assert_eq!(
        v.association_roles("ControlPointMonitoring").unwrap().len(),
        2
    );
    let ds = s129(
        r##"<UnderKeelClearancePlan gml:id="PLAN">
  <theComponent xlink:href="#CP"/>
  <theOrphan xlink:href="#CP"/>
  <theMonitored xlink:href="#CP"/>
</UnderKeelClearancePlan>
<UnderKeelClearanceControlPoint gml:id="CP">
  <theCollection xlink:href="#PLAN"/>
  <theMonitored xlink:href="#PLAN"/>
  <theComponent xlink:href="#PLAN"/>
</UnderKeelClearanceControlPoint>"##,
    );
    let issues: Vec<_> = v
        .validate(&ds, None, &S129)
        .into_iter()
        .filter(|i| {
            ["/4", "/5", "/6", "/7", "/8", "/9"]
                .iter()
                .any(|n| i.check == format!("S-158-129{n}"))
        })
        .collect();
    let found: Vec<_> = issues
        .iter()
        .map(|i| (i.check.as_str(), i.object.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(
        found,
        [
            ("S-158-129/6", "PLAN"), // theOrphan: in no association
            ("S-158-129/4", "PLAN"), // theMonitored: plan is not monitored
            ("S-158-129/9", "CP"),   // CP's monitoring role on the composition
            ("S-158-129/5", "CP"),   // composition role, wrong end
        ],
        "{issues:#?}"
    );
    assert!(issues.iter().all(|i| i.severity == Severity::Warning));
    // The released S-129 catalogue: both roles belong to every association,
    // so a known role can only be the wrong end (5) or target (8).
    let Some(plain) = common::fc(S129_FC) else {
        return;
    };
    let issues = Validator::new(&plain).validate(&ds, None, &S129);
    let c = checks(&issues);
    assert_eq!(c.get("S-158-129/7"), Some(&3), "{c:?}");
    assert_eq!(c.get("S-158-129/5"), Some(&1), "{c:?}");
    for n in [4, 6, 9] {
        assert_eq!(count(&issues, &format!("S-158-129/{n}")), 0, "{c:?}");
    }
}
