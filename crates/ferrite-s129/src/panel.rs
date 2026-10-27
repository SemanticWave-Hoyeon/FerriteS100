//! Data model of the S-129 under keel clearance plan panel: one row per
//! loaded plan with its control points. UI crates draw it.
use ferrite_s100_gml::overlay::OverlayCore;
use ferrite_s100_gml::{Object, Product};

/// A control point of an S-129 plan.
#[derive(Debug, Clone, Default)]
pub struct ControlPointRow {
    pub name: String,
    pub eta: String,
    pub window: String,
    pub speed: String,
    pub margin: String,
}

/// One loaded S-129 dataset.
#[derive(Debug, Clone, Default)]
pub struct UkcPlanRow {
    pub dataset: u32,
    pub name: String,
    pub vessel: String,
    pub draught: String,
    pub purpose: String,
    pub generation_time: String,
    pub validity: String,
    pub route: String,
    pub control_points: Vec<ControlPointRow>,
    pub issues: usize,
}

/// Rows of the loaded S-129 plans, in load order.
pub fn ukc_rows(core: &OverlayCore) -> Vec<UkcPlanRow> {
    let mut out = Vec::new();
    for entry in core
        .datasets
        .iter()
        .filter(|d| d.product() == Product::S129)
    {
        let ds = &entry.dataset;
        let plan = ds
            .objects_of("UnderKeelClearancePlan")
            .next()
            .map(|(_, o)| o);
        let v = |path: &[&str]| plan.and_then(|p| p.value(path)).unwrap_or("").to_owned();
        let label = |code: &str| {
            plan.and_then(|p| p.attribute(code))
                .and_then(|a| a.text())
                .filter(|t| !t.trim().is_empty())
                .map(|t| t.trim().to_owned())
                .unwrap_or_else(|| v(&[code]))
        };
        let window = |o: &Object| match (
            o.value(&["fixedTimeRange", "timeStart"]),
            o.value(&["fixedTimeRange", "timeEnd"]),
        ) {
            (None, None) => String::new(),
            (a, b) => format!("{} – {}", a.unwrap_or("…"), b.unwrap_or("…")),
        };
        let mut cps: Vec<(String, ControlPointRow)> = ds
            .objects_of("UnderKeelClearanceControlPoint")
            .map(|(_, o)| {
                let eta = o.value(&["expectedPassingTime"]).unwrap_or("").to_owned();
                (
                    eta.clone(),
                    ControlPointRow {
                        name: o
                            .value(&["featureName", "name"])
                            .unwrap_or(&o.id)
                            .to_owned(),
                        eta,
                        window: window(o),
                        speed: o
                            .value(&["expectedPassingSpeed"])
                            .map(|s| format!("{s} kn"))
                            .unwrap_or_default(),
                        margin: o
                            .value(&["distanceAboveUKCLimit"])
                            .map(|s| format!("{s} m"))
                            .unwrap_or_default(),
                    },
                )
            })
            .collect();
        cps.sort_by(|a, b| a.0.cmp(&b.0));
        out.push(UkcPlanRow {
            dataset: entry.id,
            name: entry.name(),
            vessel: v(&["vesselID"]),
            draught: plan
                .and_then(|p| p.value(&["maximumDraught"]))
                .map(|d| format!("{d} m"))
                .unwrap_or_default(),
            purpose: label("underKeelClearancePurpose"),
            generation_time: v(&["generationTime"]),
            validity: plan.map(window).unwrap_or_default(),
            route: [v(&["sourceRouteName"]), v(&["sourceRouteVersion"])]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            control_points: cps.into_iter().map(|(_, r)| r).collect(),
            issues: entry.issues.len(),
        });
    }
    out
}
