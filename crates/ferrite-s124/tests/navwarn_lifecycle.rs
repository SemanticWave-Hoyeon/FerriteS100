use chrono::{DateTime, Utc};
use ferrite_s100_gml::{read_bytes, read_file, Dataset, ReadLimits};
use ferrite_s124::navwarn::*;
use std::path::Path;

fn t(s: &str) -> DateTime<Utc> {
    parse_date_time(s).unwrap()
}

fn msi(series: &str, number: u32, year: u32, wtype: u8) -> String {
    format!(
        "<S124:messageSeriesIdentifier><S124:agencyResponsibleForProduction>UKHO</S124:agencyResponsibleForProduction>\
         <S124:nameOfSeries>{series}</S124:nameOfSeries><S124:warningNumber>{number}</S124:warningNumber>\
         <S124:warningType code=\"{wtype}\">x</S124:warningType><S124:year>{year}</S124:year></S124:messageSeriesIdentifier>"
    )
}

type Id<'a> = (&'a str, u32, u32, u8);

struct Spec<'a> {
    id: Id<'a>,
    publication: &'a str,
    cancellation: Option<&'a str>,
    /// (category, noMessageOnHand, referenced ids)
    refs: Vec<(u8, bool, Vec<Id<'a>>)>,
    parts: &'a str,
}

fn spec(series: &'static str, number: u32, publication: &'static str) -> Spec<'static> {
    Spec {
        id: (series, number, 26, 2),
        publication,
        cancellation: None,
        refs: Vec::new(),
        parts: PART,
    }
}

const PART: &str = "<S124:NavwarnPart gml:id=\"P1\"><S124:warningInformation><S124:information><S124:language>eng</S124:language>\
<S124:text>BUOY OFF STATION</S124:text></S124:information></S124:warningInformation><S124:header xlink:href=\"#PRE\"/>\
<S124:geometry><S100:pointProperty><S100:Point gml:id=\"G1\"><gml:pos>50.0 -1.0</gml:pos></S100:Point></S100:pointProperty></S124:geometry></S124:NavwarnPart>";

fn dataset(s: &Spec) -> Dataset {
    let mut refs = String::new();
    let mut ref_links = String::new();
    for (k, (cat, nmoh, ids)) in s.refs.iter().enumerate() {
        ref_links.push_str(&format!("<S124:theReferences xlink:href=\"#R{k}\"/>"));
        refs.push_str(&format!("<S124:References gml:id=\"R{k}\">"));
        for (se, n, y, w) in ids {
            refs.push_str(&msi(se, *n, *y, *w));
        }
        refs.push_str(&format!(
            "<S124:noMessageOnHand>{nmoh}</S124:noMessageOnHand><S124:referenceCategory code=\"{cat}\">c</S124:referenceCategory>\
             <S124:theWarning xlink:href=\"#PRE\"/></S124:References>"
        ));
    }
    let (se, n, y, w) = s.id;
    let cancel = s
        .cancellation
        .map(|c| format!("<S124:cancellationDate>{c}</S124:cancellationDate>"))
        .unwrap_or_default();
    let xml = format!(
        "<?xml version=\"1.0\"?><S124:Dataset gml:id=\"DS\" xmlns:S124=\"http://www.iho.int/S124/gml/2.0\" \
         xmlns:S100=\"http://www.iho.int/s100gml/5.0\" xmlns:gml=\"http://www.opengis.net/gml/3.2\" \
         xmlns:xlink=\"http://www.w3.org/1999/xlink\"><S100:DatasetIdentificationInformation>\
         <S100:productIdentifier>S-124</S100:productIdentifier></S100:DatasetIdentificationInformation><S124:members>\
         <S124:NavwarnPreamble gml:id=\"PRE\">{msi}<S124:navwarnTitle><S124:language>eng</S124:language><S124:text>TITLE {n}</S124:text></S124:navwarnTitle>\
         {cancel}<S124:intService>true</S124:intService><S124:navwarnTypeGeneral code=\"1\">Aids to Navigation Changes</S124:navwarnTypeGeneral>\
         <S124:publicationTime>{pubt}</S124:publicationTime>{ref_links}</S124:NavwarnPreamble>{refs}{parts}</S124:members></S124:Dataset>",
        msi = msi(se, n, y, w),
        pubt = s.publication,
        parts = s.parts,
    );
    read_bytes(
        xml.as_bytes(),
        &[&ferrite_s124::rules::IDENTITY],
        None,
        ReadLimits::default(),
    )
    .unwrap()
}

fn eval(ds: &[Dataset], now: &str) -> NavwarnRegistry {
    let inputs: Vec<NavwarnInput> = ds
        .iter()
        .map(|d| NavwarnInput {
            dataset: d,
            received: t("2026-02-13T09:00:00Z"),
        })
        .collect();
    evaluate(&inputs, t(now))
}

#[test]
fn real_ukho_datasets_are_active() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    let ds: Vec<Dataset> = names
        .iter()
        .map(|p| {
            read_file(
                p,
                &[&ferrite_s124::rules::IDENTITY],
                None,
                ReadLimits::default(),
            )
            .unwrap()
        })
        .collect();
    let r = eval(&ds, "2026-02-14T00:00:00Z");
    assert_eq!(r.messages.len(), 5);
    let n43 = r
        .messages
        .iter()
        .find(|m| m.identity.as_ref().unwrap().number == Some(43))
        .unwrap();
    assert_eq!(n43.identity.as_ref().unwrap().to_string(), "WZ 43/26");
    assert_eq!(
        n43.identity.as_ref().unwrap().nationality.as_deref(),
        Some("GB")
    );
    assert_eq!(n43.warning_type, Some(2));
    assert_eq!(n43.title.as_deref(), Some("OUTER SPIT BUOY OFF STATION."));
    assert_eq!(n43.publication_time, Some(t("2026-02-13T08:29:19Z")));
    assert_eq!(n43.general_area, vec!["WIGHT"]);
    assert_eq!(n43.locality, vec!["PORTSMOUTH APPROACHES"]);
    assert_eq!(
        n43.navwarn_type_general.as_deref(),
        Some("Aids to Navigation Changes")
    );
    assert_eq!(n43.parts[0].abbreviated(80), "Buoy off Position");
    for m in &r.messages {
        assert_eq!(m.status, Status::Active, "{}", m.caption());
    }
    // N44 has a NavwarnAreaAffected: suppressed until its part is selected.
    let i44 = r
        .messages
        .iter()
        .position(|m| m.identity.as_ref().unwrap().number == Some(44))
        .unwrap();
    assert_eq!(r.messages[i44].parts[0].areas_affected, vec!["P1_A1"]);
    let state = NavwarnUserState::default();
    let policy = PortrayalPolicy::default();
    let f = r.feature_filter(i44, &state, &policy, &[]);
    assert!(f.suppress.contains("P1_A1") && !f.suppress.contains("P1"));
    let f = r.feature_filter(i44, &state, &policy, &["P1"]);
    assert!(f.highlight.contains("P1_A1") && f.highlight.contains("P1"));
    assert!(f.suppress.is_empty());
    // Picking the area selects its part as well.
    let f = r.feature_filter(i44, &state, &policy, &["P1_A1"]);
    assert!(f.highlight.contains("P1"));
    assert!(r.bulletins.is_empty());
}

#[test]
fn cancellation_date_and_reference() {
    let mut a = spec("WZ", 10, "2026-02-01T00:00:00Z");
    a.cancellation = Some("20260205T000000Z");
    let b = spec("WZ", 11, "2026-02-01T00:00:00Z");
    let mut c = spec("WZ", 12, "2026-02-03T00:00:00Z");
    c.refs = vec![(1, false, vec![("WZ", 11, 2026, 2)])];
    let ds = [dataset(&a), dataset(&b), dataset(&c)];

    let r = eval(&ds, "2026-02-04T23:59:59Z");
    assert_eq!(r.messages[0].status, Status::Active);
    assert_eq!(
        r.messages[1].status,
        Status::Cancelled(CancelReason::CancelledBy { dataset: 2 })
    );
    assert_eq!(r.messages[2].status, Status::Active);
    assert_eq!(r.messages[2].cancels[0].to_string(), "WZ 11/26");

    let r = eval(&ds, "2026-02-05T00:00:00Z");
    assert_eq!(
        r.messages[0].status,
        Status::Cancelled(CancelReason::CancellationDate(t("2026-02-05T00:00:00Z")))
    );
    // Cancelled datasets are not portrayed but stay listed.
    let state = NavwarnUserState::default();
    assert!(!r.should_portray(0, "P1", &state, &PortrayalPolicy::default()));
    assert!(r
        .feature_filter(1, &state, &PortrayalPolicy::default(), &["P1"])
        .highlight
        .is_empty());
    assert_eq!(r.sorted(SortOrder::Received, &state).len(), 3);
}

#[test]
fn cancellation_only_dataset() {
    let mut c = spec("WZ", 20, "2026-02-03T00:00:00Z");
    c.parts = "";
    c.refs = vec![(1, false, vec![("WZ", 5, 26, 2)])];
    let target = spec("WZ", 5, "2026-01-01T00:00:00Z");
    let r = eval(&[dataset(&c), dataset(&target)], "2026-02-04T00:00:00Z");
    assert_eq!(r.messages[0].status, Status::CancellationOnly);
    // Two-digit year 26 matches 2026.
    assert_eq!(
        r.messages[1].status,
        Status::Cancelled(CancelReason::CancelledBy { dataset: 0 })
    );
}

#[test]
fn in_force_bulletin_fail_safe() {
    let old = spec("WZ", 30, "2026-01-01T00:00:00Z"); // prior, not listed -> cancelled
    let kept = spec("WZ", 31, "2026-01-02T00:00:00Z"); // prior, listed
    let newer = spec("WZ", 33, "2026-02-10T00:00:00Z"); // after bulletin
    let other_series = spec("NAVAREA I", 7, "2026-01-01T00:00:00Z");
    let mut b1 = spec("WZ", 32, "2026-01-15T00:00:00Z");
    b1.id.3 = 11;
    b1.parts = "";
    b1.refs = vec![(
        3,
        false,
        vec![("WZ", 30, 26, 2), ("WZ", 31, 26, 2), ("WZ", 32, 26, 11)],
    )];
    let mut b2 = spec("WZ", 34, "2026-02-01T00:00:00Z");
    b2.id.3 = 11;
    b2.parts = "";
    b2.refs = vec![(
        3,
        false,
        vec![
            ("WZ", 31, 26, 2),
            ("WZ", 34, 26, 11),
            ("WZ", 29, 26, 2),
            ("WZ", 28, 26, 2),
        ],
    )];
    let ds = [
        dataset(&old),
        dataset(&kept),
        dataset(&newer),
        dataset(&other_series),
        dataset(&b1),
        dataset(&b2),
    ];
    let r = eval(&ds, "2026-02-11T00:00:00Z");
    assert_eq!(
        r.messages[0].status,
        Status::Cancelled(CancelReason::NotInForceBulletin { bulletin: 5 })
    );
    assert_eq!(r.messages[1].status, Status::Active);
    assert_eq!(r.messages[2].status, Status::Active);
    assert_eq!(r.messages[3].status, Status::Active);
    assert_eq!(
        r.messages[4].status,
        Status::Cancelled(CancelReason::SupersededInForceBulletin { by: 5 })
    );
    assert_eq!(r.messages[5].status, Status::InForceBulletin);
    assert_eq!(r.bulletins.len(), 1);
    let b = &r.bulletins[0];
    assert_eq!(b.bulletin, 5);
    let missing: Vec<String> = b.missing.iter().map(|m| m.id.to_string()).collect();
    assert_eq!(missing, vec!["WZ 29/26", "WZ 28/26"]);
    assert_eq!(r.missing_indications().count(), 1);
    // Bulletins are not portrayed.
    assert!(!r.dataset_portrayed(5, &NavwarnUserState::default(), &PortrayalPolicy::default()));
}

#[test]
fn no_message_on_hand_cancels_whole_series() {
    let a = spec("WZ", 40, "2026-01-01T00:00:00Z");
    let mut b = spec("WZ", 41, "2026-01-10T00:00:00Z");
    b.id.3 = 11;
    b.parts = "";
    b.refs = vec![(3, true, vec![])];
    let r = eval(&[dataset(&a), dataset(&b)], "2026-01-11T00:00:00Z");
    assert!(r.messages[1].no_message_on_hand);
    assert_eq!(
        r.messages[0].status,
        Status::Cancelled(CancelReason::NotInForceBulletin { bulletin: 1 })
    );
    assert!(r.bulletins[0].missing.is_empty());
}

fn part_with_range(range: &str) -> String {
    PART.replace(
        "<S124:header",
        &format!("<S124:fixedDateRange>{range}</S124:fixedDateRange><S124:header"),
    )
}

#[test]
fn fixed_date_range_example_1() {
    // S-124 11.1.2 EXAMPLE 1.
    let parts = part_with_range(
        "<S124:dateEnd>2023-07-10</S124:dateEnd><S124:dateStart>2023-07-06</S124:dateStart>\
         <S124:timeOfDayEnd>01:00:00Z</S124:timeOfDayEnd><S124:timeOfDayStart>01:00:00Z</S124:timeOfDayStart>",
    )
    .replace("<S124:geometry>", "<S124:theCartographicText xlink:href=\"#T1\"/><S124:geometry>");
    let parts = format!(
        "{parts}<S124:TextPlacement gml:id=\"T1\"><S124:text>X</S124:text><S124:textOffsetBearing>0</S124:textOffsetBearing>\
         <S124:textOffsetDistance>0</S124:textOffsetDistance><S124:textRotation>0</S124:textRotation>\
         <S124:thePositionProvider xlink:href=\"#P1\"/><S124:geometry><S100:pointProperty><S100:Point gml:id=\"G2\">\
         <gml:pos>50.0 -1.0</gml:pos></S100:Point></S100:pointProperty></S124:geometry></S124:TextPlacement>"
    );
    let mut s = spec("WZ", 50, "20230704T010000Z");
    s.id.2 = 2023;
    s.cancellation = Some("20230711T000000Z");
    s.parts = &parts;
    let ds = [dataset(&s)];
    let state = NavwarnUserState::default();
    let show = PortrayalPolicy::default();
    let strict = PortrayalPolicy {
        show_before_fixed_date_range: false,
        ..Default::default()
    };

    // Before the range: optional.
    let r = eval(&ds, "2023-07-05T00:00:00Z");
    assert_eq!(r.messages[0].status, Status::NotYetActive);
    assert_eq!(r.messages[0].parts[0].text_placements, vec!["T1"]);
    assert!(r.should_portray(0, "P1", &state, &show));
    assert!(!r.should_portray(0, "P1", &state, &strict));
    let f = r.feature_filter(0, &state, &strict, &[]);
    assert!(f.suppress.contains("P1") && f.suppress.contains("T1"));

    // Inside (inclusive bounds): mandatory.
    for now in [
        "2023-07-06T01:00:00Z",
        "2023-07-08T00:00:00Z",
        "2023-07-10T01:00:00Z",
    ] {
        let r = eval(&ds, now);
        assert_eq!(r.messages[0].status, Status::Active);
        assert!(r.should_portray(0, "P1", &state, &strict), "{now}");
        assert!(r
            .feature_filter(0, &state, &strict, &[])
            .suppress
            .is_empty());
    }
    // After the range, before cancellation: hidden.
    let r = eval(&ds, "2023-07-10T01:00:01Z");
    assert!(matches!(
        r.messages[0].parts[0].timing,
        PartTiming::AfterRange { .. }
    ));
    assert!(!r.should_portray(0, "P1", &state, &show));
    // After cancellation.
    let r = eval(&ds, "2023-07-11T00:00:00Z");
    assert!(r.messages[0].status.is_cancelled());
}

#[test]
fn date_only_range_includes_whole_end_day() {
    let parts = part_with_range(
        "<S124:dateEnd>20211104</S124:dateEnd><S124:dateStart>20211101</S124:dateStart>",
    );
    let mut s = spec("WZ", 60, "2021-10-01T00:00:00Z");
    s.parts = &parts;
    let ds = [dataset(&s)];
    let at = |now: &str| eval(&ds, now).messages[0].parts[0].timing;
    assert!(matches!(
        at("2021-10-31T23:59:59Z"),
        PartTiming::BeforeRange { .. }
    ));
    assert_eq!(at("2021-11-01T00:00:00Z"), PartTiming::InRange);
    assert_eq!(at("2021-11-04T23:59:59Z"), PartTiming::InRange);
    assert!(matches!(
        at("2021-11-05T00:00:00Z"),
        PartTiming::AfterRange { .. }
    ));
}

#[test]
fn classification_new_indication_and_sorting() {
    let a = spec("WZ", 70, "2026-02-01T00:00:00Z");
    let b = spec("AB", 1, "2026-02-02T00:00:00Z");
    let ds = [dataset(&a), dataset(&b)];
    let inputs = [
        NavwarnInput {
            dataset: &ds[0],
            received: t("2026-02-10T00:00:00Z"),
        },
        NavwarnInput {
            dataset: &ds[1],
            received: t("2026-02-09T00:00:00Z"),
        },
    ];
    let r = evaluate(&inputs, t("2026-02-10T12:00:00Z"));
    let mut state = NavwarnUserState::default();
    let policy = PortrayalPolicy::default();
    assert!(r.is_new(0, &state, true));
    assert!(!r.is_new(0, &state, false));
    assert!(!r.is_new(1, &state, true)); // more than 24 h
    state.mark_displayed(&r.messages[0]);
    assert!(!r.is_new(0, &state, true));

    assert_eq!(
        state.classification_of(&r.messages[0]),
        UserClassification::OnChart
    );
    assert!(r.should_portray(0, "P1", &state, &policy));
    state.classify(&r.messages[0], UserClassification::OffChart);
    assert!(!r.should_portray(0, "P1", &state, &policy));
    state.classify(&r.messages[0], UserClassification::Information);
    assert!(!r.should_portray(0, "P1", &state, &policy));
    assert!(r.should_portray(
        0,
        "P1",
        &state,
        &PortrayalPolicy {
            show_information: true,
            ..policy
        }
    ));

    assert_eq!(r.sorted(SortOrder::Received, &state), vec![0, 1]);
    assert_eq!(r.sorted(SortOrder::IssueTime, &state), vec![1, 0]);
    assert_eq!(r.sorted(SortOrder::ProducerSeries, &state), vec![1, 0]);
    assert_eq!(r.sorted(SortOrder::UserClassification, &state), vec![1, 0]);
    assert_eq!(r.sorted(SortOrder::WarningType, &state), vec![1, 0]);
}
