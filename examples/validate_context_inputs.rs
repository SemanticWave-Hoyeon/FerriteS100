//! Audit delivered PC inputs, raw rules, disclosed S-101 remediations and candidate settings.
use anyhow::{ensure, Result};
use ferrite_lua::ContextParameters;
use ferrite_portrayal_catalog::{validate_context_values, PortrayalCatalogue};
use ferrite_s101::{synchronize_legacy_context, validate_portrayal_context};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf};
fn main() -> Result<()> {
    let base = PathBuf::from(std::env::args().nth(1).expect("catalogue versions folder"));
    let mut rows = Vec::new();
    for version in ["1.0.2", "1.1.0", "2.0.0", "2.1.0"] {
        let pc = PortrayalCatalogue::load(base.join(version).join("PC"))?;
        let mut context = ContextParameters::from_pc_context(pc.get_context_parameters());
        synchronize_legacy_context(&pc, &mut context);
        let values: HashMap<_, _> = context
            .to_lua_params()
            .into_iter()
            .map(|(id, _, v)| (id, v))
            .collect();
        let raw = validate_context_values(pc.get_context_parameters(), &values);
        let raw_ok = raw.as_ref().is_ok_and(|r| r.failures.is_empty());
        let (report, diagnostics) = validate_portrayal_context(&pc, &context)?;
        ensure!(
            raw_ok == diagnostics.is_empty(),
            "Raw PC validation result did not expose language regex defect"
        );
        let parameter_count = pc.get_context_parameters().len();
        let rule_count = pc
            .get_context_parameters()
            .values()
            .map(|p| p.validations.len())
            .sum::<usize>();
        let enabled_count = pc
            .get_context_parameters()
            .values()
            .filter(|p| p.enable.is_some())
            .count();
        // Effective UI four-shade input: these changes must fail before portrayal, including legacy names.
        context.two_shades = false;
        context.safety_contour = 10.;
        context.shallow_contour = 12.;
        context.deep_contour = 30.;
        synchronize_legacy_context(&pc, &mut context);
        let shallow_error = validate_portrayal_context(&pc, &context)
            .unwrap_err()
            .to_string();
        ensure!(
            shallow_error.contains("ShallowContour"),
            "Wrong shallow rejection"
        );
        context.shallow_contour = 2.;
        context.deep_contour = 5.;
        synchronize_legacy_context(&pc, &mut context);
        let deep_error = validate_portrayal_context(&pc, &context)
            .unwrap_err()
            .to_string();
        ensure!(deep_error.contains("DeepContour"), "Wrong deep rejection");
        context.shallow_contour = 10.;
        context.deep_contour = 10.;
        synchronize_legacy_context(&pc, &mut context);
        validate_portrayal_context(&pc, &context)?;
        // Disabled optional contour rules must not block two-shade portrayal.
        context.two_shades = true;
        context.shallow_contour = 12.;
        context.deep_contour = 5.;
        synchronize_legacy_context(&pc, &mut context);
        validate_portrayal_context(&pc, &context)?;
        context.two_shades = false;
        context.shallow_contour = 2.;
        context.deep_contour = 30.;
        context.safety_contour = f64::NAN;
        synchronize_legacy_context(&pc, &mut context);
        ensure!(
            validate_portrayal_context(&pc, &context).is_err(),
            "Non-finite depth accepted"
        );
        rows.push(json!({"version":version,"parameters":parameter_count,"validation_rules":rule_count,"conditional_parameters":enabled_count,"raw_defaults_pass":raw_ok,"adapted_defaults_pass":true,"default_checked_rules":report.checked_rules,"local_remediations":diagnostics,"rejected_four_shade_shallow":shallow_error,"rejected_four_shade_deep":deep_error,"boundary_equal_accepted":true,"two_shade_optional_contours_disabled":true,"nonfinite_rejected":true}));
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}
