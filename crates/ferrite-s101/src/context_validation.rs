//! S-101 context bridge. The generic PC validator never rewrites catalogue expressions.
use anyhow::{ensure, Result};
use ferrite_lua::{ContextParameters, ContextValue};
use ferrite_portrayal_catalog::{
    validate_context_values, ContextExpression, ContextParameter, ContextValidationReport,
    PortrayalCatalogue,
};
use std::collections::HashMap;
/// Known delivered language-pattern defects are remediated explicitly in the product adapter.
/// Original catalogue files and parsed expressions remain unchanged. Strict validation stays available.
pub fn context_validation_parameters(
    pc: &PortrayalCatalogue,
) -> Result<(HashMap<String, ContextParameter>, Vec<String>)> {
    ensure!(
        pc.product_id == "S-101",
        "S-101 context adapter received a different product"
    );
    let mut parameters = pc.get_context_parameters().clone();
    let mut diagnostics = Vec::new();
    for (id, p) in &mut parameters {
        for rule in &mut p.validations {
            let ContextExpression::Regex(source) = &mut rule.expression else {
                continue;
            };
            let replacement = match (pc.version.as_str(), id.as_str(), source.as_str()) {
                ("1.0.2" | "2.0.0", "NationalLanguage", "^[a-z]{3}$") => Some("[a-z]{3}"),
                ("2.1.0", "PreferredLanguage", r"^[a-z]{3}(?:\s*,\s*[a-z]{3})*$") => {
                    Some(r"[a-z]{3}(\s*,\s*[a-z]{3})*")
                }
                _ => None,
            };
            if let Some(replacement) = replacement {
                diagnostics.push(format!("S-101 PC {} {id}: local XML-regex remediation {source:?} -> {replacement:?}; NationalLanguage anchor defect documented at https://github.com/iho-ohi/S-101_Portrayal-Catalogue/issues/467 ; original file retained, not an IHO-approved revised catalogue",pc.version));
                *source = replacement.into();
            }
        }
    }
    diagnostics.sort();
    Ok((parameters, diagnostics))
}
/// Legacy PC parameter names use the same effective UI inputs, rather than their old defaults.
pub fn synchronize_legacy_context(pc: &PortrayalCatalogue, context: &mut ContextParameters) {
    for (id, value) in [
        ("TwoShades", context.two_shades),
        (
            "ShowIsolatedDangersInShallowWaters",
            context.shallow_water_dangers,
        ),
        ("SimplifiedPoints", context.simplified_symbols),
        ("FullSectors", context.full_sectors),
        ("IgnoreScamin", context.ignore_scamin),
    ] {
        if pc.get_context_parameters().contains_key(id) {
            context.custom.insert(id.into(), ContextValue::Bool(value));
        }
    }
}
pub fn validate_portrayal_context(
    pc: &PortrayalCatalogue,
    context: &ContextParameters,
) -> Result<(ContextValidationReport, Vec<String>)> {
    let (parameters, diagnostics) = context_validation_parameters(pc)?;
    let values: HashMap<_, _> = context
        .to_lua_params()
        .into_iter()
        .map(|(id, _, v)| (id, v))
        .collect();
    let report = validate_context_values(&parameters, &values)?;
    report.ensure_valid()?;
    Ok((report, diagnostics))
}
