//! The shipped S-124 and S-129 portrayal rules run on Part 9 input.
use ferrite_xslt::{parse, serialize, Limits, Stylesheet};
use std::path::PathBuf;

fn rules(product: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../Catalogues/PC")
        .join(product)
        .join("Rules")
}

fn transform(product: &str, input: &str, params: &[(&str, &str)]) -> String {
    let dir = rules(product);
    let sheet = Stylesheet::load(&dir.join("main.xsl"), &dir, Limits::default()).unwrap();
    let out = sheet
        .transform(parse(input, None, None).unwrap(), params)
        .unwrap();
    serialize(&out.root)
}

#[test]
fn s124_navwarn_parts() {
    let out = transform(
        "S-124",
        r#"<Dataset><Points><Point id="p1"/></Points><Features>
            <NavwarnPart id="a" primitive="Point"><Point ref="p1"/></NavwarnPart>
            <NavwarnPart id="b" primitive="Point"><Point ref="p1"/></NavwarnPart>
            <NavwarnPart id="c" primitive="Surface"><restriction>1</restriction></NavwarnPart>
            <NavwarnPart id="d" primitive="Curve"/>
            <TextPlacement id="t" primitive="Point"><text>Hello</text></TextPlacement>
        </Features></Dataset>"#,
        &[],
    );
    assert!(
        out.starts_with("<displayList><pointInstruction><featureReference>a</featureReference>"),
        "{out}"
    );
    assert!(
        out.contains("<text>2</text><bodySize>10</bodySize><foreground>CHMGD</foreground>"),
        "{out}"
    );
    assert_eq!(
        out.matches("<featureReference>b</featureReference>")
            .count(),
        0,
        "{out}"
    );
    assert!(
        out.contains("<lineStyleReference reference=\"NavigationalWarningFeaturePartLine\"/>"),
        "{out}"
    );
    assert!(out.contains("<text>Hello</text>"), "{out}");
}

#[test]
fn s129_features_and_virtual_inputs() {
    let input = r#"<Dataset><Features>
        <UnderKeelClearancePlanArea id="pa" primitive="Surface"/>
        <UnderKeelClearanceNonNavigableArea id="nn" primitive="Surface"/>
        <UnderKeelClearanceAlmostNonNavigableArea id="an" primitive="Surface"/>
        <UnderKeelClearanceControlPoint id="cp" primitive="Point"/>
        <PlannedRoute id="r" primitive="Curve"/>
        <UnderKeelClearanceControlPointInformationBox id="ib" primitive="Point"><name>CP1</name><ETA>2024-04-17T22:05:00Z</ETA><TimeWindowOpen>2024-04-17T21:00:00Z</TimeWindowOpen><TimeWindowClose>2024-04-17T23:30:00Z</TimeWindowClose></UnderKeelClearanceControlPointInformationBox>
    </Features></Dataset>"#;
    let plain = transform("S-129", input, &[("PlainBoundaries", "true")]);
    assert!(
        plain.contains("<intervalLength>3.18</intervalLength>"),
        "{plain}"
    );
    let symbolized = transform("S-129", input, &[("PlainBoundaries", "false")]);
    assert!(
        symbolized.contains("lineStyleReference reference=\"UKCARE01\""),
        "{symbolized}"
    );
    assert!(
        plain.contains("<color transparency=\"0.5\">RED</color>"),
        "{plain}"
    );
    assert_eq!(plain.matches("<symbolFill>").count(), 2, "{plain}");
    assert!(plain.contains("<symbol reference=\"UKCCONPT\">"), "{plain}");
    assert!(plain.contains("<token>PLRTE</token>"), "{plain}");
    assert!(
        plain.contains("<text>CP1\nETA:   17/2205\nOpen:  17/2100\nClose: 17/2330</text>"),
        "{plain}"
    );
}
