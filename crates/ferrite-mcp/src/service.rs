//! Product-independent S-100 registry with explicit dataset-scoped adapters.
use crate::Indices;
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

pub struct Dataset {
    pub id: String,
    pub product: String,
    pub metadata: Value,
    pub s101: Option<Arc<Indices>>,
}
#[derive(Default)]
pub struct Registry {
    datasets: BTreeMap<String, Dataset>,
}
impl Registry {
    pub fn new(datasets: impl IntoIterator<Item = Dataset>) -> Result<Self> {
        let mut out = Self::default();
        for d in datasets {
            ensure!(!d.id.is_empty() && d.id.len() <= 4096, "invalid dataset id");
            ensure!(
                d.s101.is_none() || d.product == "S-101",
                "S-101 adapter/product mismatch"
            );
            ensure!(
                !out.datasets.contains_key(&d.id),
                "duplicate dataset id: {}",
                d.id
            );
            out.datasets.insert(d.id.clone(), d);
        }
        Ok(out)
    }
    pub fn len(&self) -> usize {
        self.datasets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.datasets.is_empty()
    }
    pub fn list(&self) -> Value {
        json!({"datasets": self.datasets.values().map(|d| json!({"dataset_id":d.id,"product":d.product,"metadata":d.metadata,"query_adapter":if d.s101.is_some(){"S-101"}else{"metadata-only"}})).collect::<Vec<_>>()})
    }
    fn select(&self, args: &Value) -> Result<&Dataset> {
        if let Some(id) = args.get("dataset_id").and_then(Value::as_str) {
            return self
                .datasets
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("unknown dataset_id"));
        }
        ensure!(
            self.datasets.len() == 1,
            "dataset_id required: use datasets_list to select a dataset"
        );
        self.datasets
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no dataset loaded"))
    }
    pub fn call(&self, name: &str, mut args: Value) -> Result<Value> {
        validate_args(&args)?;
        if name == "catalogue_search" {
            ensure!(
                args.get("limit")
                    .and_then(Value::as_u64)
                    .is_none_or(|n| n <= 200),
                "catalogue search limit must be 1..200"
            );
        }
        match name {
            "datasets_list" => return Ok(self.list()),
            "s100_capabilities" => {
                return Ok(
                    json!({"read_only":true,"common":["datasets_list","dataset_metadata","s100_capabilities"],"product_queries":{"S-101":"catalogue, feature and approximate spatial queries","S-102":"metadata-only","S-421":"metadata-only when loaded by host"},"portrayal_or_navigation_control":false}),
                )
            }
            _ => {}
        }
        let dataset = self.select(&args)?;
        if name == "dataset_metadata" {
            return Ok(
                json!({"dataset_id":dataset.id,"product":dataset.product,"metadata":dataset.metadata}),
            );
        }
        let Some(index) = &dataset.s101 else {
            bail!(
                "query '{}' is not supported for this dataset ({})",
                name,
                dataset.product
            );
        };
        if let Some(obj) = args.as_object_mut() {
            obj.remove("dataset_id");
        }
        let result = crate::mcp::tools::dispatch(index, name, args)?;
        Ok(json!({"dataset_id":dataset.id,"product":dataset.product,"result":result}))
    }
}
pub fn descriptors() -> Vec<Value> {
    let mut tools = crate::mcp::tools::list_descriptors();
    for tool in &mut tools {
        tool["inputSchema"]["properties"]["dataset_id"] = json!({"type":"string","description":"From datasets_list; required when multiple datasets are loaded. Feature identifiers are local to this dataset."});
        if tool["name"] == "dataset_metadata" {
            tool["description"] =
                json!("Read product, catalogue and source metadata for any loaded S-100 dataset.");
        }
        tool["annotations"] =
            json!({"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false});
    }
    for (name, description) in [
        (
            "datasets_list",
            "List loaded S-100 datasets, products, FC/PC metadata and adapter capability.",
        ),
        (
            "s100_capabilities",
            "Report supported S-100 common and product-specific tools.",
        ),
    ] {
        tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    }
    tools
}
fn validate_args(a: &Value) -> Result<()> {
    ensure!(a.is_object(), "arguments must be an object");
    if let Some(v) = a.get("limit") {
        ensure!(
            v.as_u64().is_some_and(|n| (1..=1000).contains(&n)),
            "limit must be 1..1000"
        );
    }
    for name in ["term", "code", "type", "dataset_id"] {
        if let Some(v) = a.get(name) {
            ensure!(
                v.as_str().is_some_and(|s| s.len() <= 4096),
                "invalid/oversized string argument"
            );
        }
    }
    if let Some(v) = a.get("lat") {
        ensure!(
            v.as_f64()
                .is_some_and(|n| n.is_finite() && (-90.0..=90.0).contains(&n)),
            "invalid latitude"
        );
    }
    if let Some(v) = a.get("lon") {
        ensure!(
            v.as_f64()
                .is_some_and(|n| n.is_finite() && (-180.0..=180.0).contains(&n)),
            "invalid longitude"
        );
    }
    if let Some(v) = a.get("radius_m") {
        ensure!(
            v.as_f64()
                .is_some_and(|n| n.is_finite() && (0.0..=1_000_000.0).contains(&n)),
            "invalid query radius"
        );
    }
    let b = a.get("bbox").unwrap_or(a);
    if ["w", "s", "e", "n"].iter().any(|k| b.get(k).is_some()) {
        let n = |k: &str| {
            b.get(k)
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite())
                .ok_or_else(|| anyhow::anyhow!("invalid bbox"))
        };
        let (w, s, e, n) = (n("w")?, n("s")?, n("e")?, n("n")?);
        ensure!(
            (-180.0..=180.0).contains(&w)
                && (-180.0..=180.0).contains(&e)
                && (-90.0..=90.0).contains(&s)
                && (-90.0..=90.0).contains(&n)
                && w <= e
                && s <= n,
            "invalid bbox (split dateline queries into two boxes)"
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn dataset(id: &str, product: &str) -> Dataset {
        Dataset {
            id: id.into(),
            product: product.into(),
            metadata: json!({"fc":"declared"}),
            s101: None,
        }
    }
    #[test]
    fn mixed_products_require_explicit_identity() {
        let r = Registry::new([
            dataset("a", "S-101"),
            dataset("b", "S-102"),
            dataset("c", "S-421"),
        ])
        .unwrap();
        assert_eq!(r.list()["datasets"].as_array().unwrap().len(), 3);
        assert!(r.call("dataset_metadata", json!({})).is_err());
        assert_eq!(
            r.call("dataset_metadata", json!({"dataset_id":"b"}))
                .unwrap()["product"],
            "S-102"
        );
        assert!(r
            .call("feature_get", json!({"dataset_id":"b","id":1}))
            .is_err());
    }
    #[test]
    fn duplicate_identity_rejected() {
        assert!(Registry::new([dataset("a", "S-101"), dataset("a", "S-102")]).is_err());
    }
    #[test]
    fn unbounded_and_invalid_queries_rejected() {
        for a in [
            json!({"limit":0}),
            json!({"limit":1001}),
            json!({"radius_m":-1}),
            json!({"lat":91}),
            json!({"w":170,"s":0,"e":-170,"n":1}),
        ] {
            assert!(validate_args(&a).is_err());
        }
    }
    #[test]
    fn no_hidden_mutating_tools() {
        let d = descriptors();
        assert_eq!(d.len(), 11);
        for t in d {
            assert_eq!(t["annotations"]["readOnlyHint"], true);
        }
    }
    #[test]
    fn unload_replaces_registry_without_retaining_data() {
        let mut r = Registry::new([dataset("a", "S-101")]).unwrap();
        assert_eq!(r.len(), 1);
        r = Registry::default();
        assert!(r
            .call("dataset_metadata", json!({"dataset_id":"a"}))
            .is_err());
    }
}
