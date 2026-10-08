//! Resolve attributes on the query copy only; rendering/source records are unchanged.
pub fn resolve_attribute_values(
    cell: &mut ferrite_s100_core::S101Cell,
    fc: &ferrite_feature_catalog::FeatureCatalogue,
) {
    use ferrite_feature_catalog::AttributeValueType;

    let mut resolved_count: u64 = 0;
    let mut unmapped: std::collections::HashSet<String> = std::collections::HashSet::new();

    let resolve_one = |code: &str, atvl: &str| -> Option<ferrite_s100_core::AttributeValue> {
        if atvl.is_empty() {
            return None;
        }
        // Look up the simple-attribute definition. ComplexAttribute
        // value containers carry no atvl themselves — their children
        // (which appear as separate ATTR rows with a higher PAIX) do.
        let sa = fc.simple_attributes.get(code)?;
        Some(match sa.value_type {
            AttributeValueType::Boolean => {
                let lo = atvl.trim().to_ascii_lowercase();
                match lo.as_str() {
                    "1" | "true" => ferrite_s100_core::AttributeValue::Boolean(true),
                    "0" | "false" => ferrite_s100_core::AttributeValue::Boolean(false),
                    _ => ferrite_s100_core::AttributeValue::Text(atvl.to_owned()),
                }
            }
            AttributeValueType::Integer => atvl
                .trim()
                .parse::<i64>()
                .map(ferrite_s100_core::AttributeValue::Integer)
                .unwrap_or_else(|_| ferrite_s100_core::AttributeValue::Text(atvl.to_string())),
            AttributeValueType::Real => atvl
                .trim()
                .parse::<f64>()
                .map(ferrite_s100_core::AttributeValue::Real)
                .unwrap_or_else(|_| ferrite_s100_core::AttributeValue::Text(atvl.to_string())),
            AttributeValueType::Enumeration | AttributeValueType::S100CodeList => {
                // S-101 encodes enumerations as the listed-value
                // numeric code, written out as ASCII digits in atvl.
                match atvl.trim().parse::<u32>() {
                    Ok(num) => {
                        let label = sa
                            .listed_values
                            .iter()
                            .find(|lv| lv.code == num)
                            .map(|lv| lv.label.clone())
                            .unwrap_or_else(|| format!("unknown_{}", num));
                        ferrite_s100_core::AttributeValue::Enumeration(num, label)
                    }
                    Err(_) => ferrite_s100_core::AttributeValue::Text(atvl.to_string()),
                }
            }
            AttributeValueType::Date => ferrite_s100_core::AttributeValue::Date(atvl.to_string()),
            AttributeValueType::Time => ferrite_s100_core::AttributeValue::Time(atvl.to_string()),
            AttributeValueType::DateTime => {
                ferrite_s100_core::AttributeValue::DateTime(atvl.to_string())
            }
            AttributeValueType::Text
            | AttributeValueType::Uri
            | AttributeValueType::Url
            | AttributeValueType::Urn
            | AttributeValueType::TruncatedDate
            | AttributeValueType::S100TruncatedDate => {
                ferrite_s100_core::AttributeValue::Text(atvl.to_string())
            }
        })
    };

    // Walk all features + information records; populate
    // `attr.value` if we can. Skip already-resolved attributes so
    // calling this twice is idempotent.
    for feature in cell.features.values_mut() {
        for attr in &mut feature.attributes {
            if attr.value.is_some() {
                continue;
            }
            let Some(code) = attr.code.as_deref() else {
                continue;
            };
            if let Some(v) = resolve_one(code, &attr.atvl) {
                attr.value = Some(v);
                resolved_count += 1;
            } else if !attr.atvl.is_empty() && !fc.simple_attributes.contains_key(code) {
                unmapped.insert(code.to_string());
            }
        }
    }
    for info in cell.information.values_mut() {
        for attr in &mut info.attributes {
            if attr.value.is_some() {
                continue;
            }
            let Some(code) = attr.code.as_deref() else {
                continue;
            };
            if let Some(v) = resolve_one(code, &attr.atvl) {
                attr.value = Some(v);
                resolved_count += 1;
            } else if !attr.atvl.is_empty() && !fc.simple_attributes.contains_key(code) {
                unmapped.insert(code.to_string());
            }
        }
    }

    tracing::info!(
        "Resolved {} attribute values via FC ({} attribute codes were not in the catalogue)",
        resolved_count,
        unmapped.len()
    );
    if !unmapped.is_empty() {
        tracing::debug!("Unmapped attribute codes: {:?}", unmapped);
    }
}
