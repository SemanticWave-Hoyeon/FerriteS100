//! The virtual input features the S-129 2.0.0 portrayal catalogue expects
//! (`PlannedRoute`, `UnderKeelClearanceControlPointInformationBox`), added to
//! the Part 9-A portrayal input through
//! [`ferrite_s100_gml::portrayal::InputOptions::virtual_features`].
use ferrite_s100_gml::portrayal::{
    InputFeature, InputOptions, InputSpatial, VirtualFeature, VirtualGeometry,
};
use ferrite_s100_gml::{Attribute, Coord, Dataset, Geometry, Object};

/// Feature code of the S-129 virtual route feature.
pub const PLANNED_ROUTE: &str = "PlannedRoute";
/// Feature code of the S-129 virtual information box feature.
pub const INFORMATION_BOX: &str = "UnderKeelClearanceControlPointInformationBox";
const CONTROL_POINT: &str = "UnderKeelClearanceControlPoint";

/// Portrayal input options of S-129 datasets: the virtual features added.
pub fn input_options() -> InputOptions {
    InputOptions {
        virtual_features: Some(virtual_features),
        ..InputOptions::default()
    }
}

/// The planned route through the control points (in passing order) and one
/// information box per control point.
pub fn virtual_features(
    dataset: &Dataset,
    features: &[InputFeature],
    spatials: &[InputSpatial],
    note: &mut dyn FnMut(Option<&str>, String),
) -> Vec<VirtualFeature> {
    let mut points: Vec<(usize, Option<chrono::DateTime<chrono::FixedOffset>>)> = Vec::new();
    for (fi, feature) in features.iter().enumerate() {
        if feature.code != CONTROL_POINT {
            continue;
        }
        let object = &dataset.objects[feature.object.expect("dataset feature")];
        let time = object
            .value(&["expectedPassingTime"])
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t.trim()).ok());
        if !matches!(object.geometry.first(), Some(Geometry::Point(_))) {
            note(Some(&object.id), "Control point without point geometry is left out of the planned route and information boxes".into());
            continue;
        }
        points.push((fi, time));
    }
    // The route visits the control points in passing order (S-129: the
    // route is "defined by a set of geographical control points with time
    // windows"); expectedPassingTime orders them. Without a time for every
    // point the dataset order is kept.
    if points.iter().all(|(_, t)| t.is_some()) {
        points.sort_by_key(|(_, t)| *t);
    } else if !points.is_empty() {
        note(None, "Not every control point has a valid expectedPassingTime; the planned route follows dataset order".into());
    }
    let mut info_boxes = Vec::new();
    for (fi, _) in &points {
        let feature = &features[*fi];
        let object = &dataset.objects[feature.object.expect("dataset feature")];
        let mut children = Vec::new();
        if let Some(name) = display_name(object) {
            children.push(("name", name));
        }
        if let Some(eta) = object.value(&["expectedPassingTime"]) {
            children.push(("ETA", eta.trim().to_owned()));
        }
        if let Some(open) = object.value(&["fixedTimeRange", "timeStart"]) {
            children.push(("TimeWindowOpen", open.trim().to_owned()));
        }
        if let Some(close) = object.value(&["fixedTimeRange", "timeEnd"]) {
            children.push(("TimeWindowClose", close.trim().to_owned()));
        }
        info_boxes.push(VirtualFeature {
            id: format!("~{INFORMATION_BOX}.{}", feature.id),
            code: INFORMATION_BOX.into(),
            primitive: "Point",
            geometry: VirtualGeometry::Spatial(feature.spatials[0]),
            scale_minimum: feature.scale_minimum,
            children,
        });
    }
    let route: Vec<Coord> = points
        .iter()
        .map(|(fi, _)| spatials[features[*fi].spatials[0]].coords[0])
        .collect();
    let mut out = Vec::with_capacity(info_boxes.len() + 1);
    if route.len() >= 2 {
        out.push(VirtualFeature {
            id: format!("~{PLANNED_ROUTE}"),
            code: PLANNED_ROUTE.into(),
            primitive: "Curve",
            geometry: VirtualGeometry::Curve(route),
            scale_minimum: None,
            children: Vec::new(),
        });
    } else if !points.is_empty() {
        note(
            None,
            "Fewer than two control points; no planned route".into(),
        );
    }
    out.extend(info_boxes);
    out
}

/// featureName/name with nameUsage 1 (default display), else the first.
fn display_name(object: &Object) -> Option<String> {
    let names: Vec<&Attribute> = object.attributes_named("featureName").collect();
    let pick = names
        .iter()
        .find(|n| {
            n.children()
                .iter()
                .any(|c| c.code == "nameUsage" && c.simple_value().map(str::trim) == Some("1"))
        })
        .or(names.first())?;
    pick.children()
        .iter()
        .find(|c| c.code == "name")
        .and_then(|c| c.text())
        .map(|t| t.trim().to_owned())
}
