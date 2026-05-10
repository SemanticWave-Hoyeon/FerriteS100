//! 9 read-only tools per plan2 §4.2.
//!
//! All tools return JSON whose top-level shape always includes an
//! `"evidence"` array — even when empty — so M6 (evidence-constrained)
//! prompts can demand callers cite at least one entry. This is the
//! mechanism plan2 §13 leans on to suppress hallucination.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use ferrite_feature_catalog::{ComplexAttribute, FeatureType, SimpleAttribute};
use ferrite_s100_core::{AttributeValue, Coordinate, FeatureRecord};

use crate::indices::evidence::{catalogue_definition_for_feature, EvidenceRef};
use crate::indices::feature::FeatureId;
use crate::indices::geometry::Bbox;
use crate::indices::Indices;

// --- Tool descriptors (for `tools/list`) --------------------------------

pub fn list_descriptors() -> Vec<Value> {
    vec![
        descriptor(
            "dataset_metadata",
            "Return the loaded S-101 cell's identification, scale, extent, and feature-type histogram.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        descriptor(
            "catalogue_search",
            "Substring search across Feature Catalogue feature types, simple attributes, complex attributes, and information types.",
            json!({
                "type": "object",
                "properties": {
                    "term": { "type": "string", "description": "Search term (case-insensitive)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 25 }
                },
                "required": ["term"],
                "additionalProperties": false
            }),
        ),
        descriptor(
            "catalogue_describe_feature",
            "Return the catalogue definition for a feature type code (e.g. 'BuoyLateral').",
            json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Feature type code" }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        ),
        descriptor(
            "catalogue_describe_attribute",
            "Return the catalogue definition for an attribute (simple or complex) by code.",
            json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Attribute code" }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        ),
        descriptor(
            "feature_get",
            "Return geometry summary and attributes for a feature id.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Feature id (FRID rcid)" }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        ),
        descriptor(
            "feature_query_by_type",
            "Find features whose code matches `type` (case-insensitive), optionally restricted to a bbox.",
            json!({
                "type": "object",
                "properties": {
                    "type": { "type": "string" },
                    "bbox": bbox_schema(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 }
                },
                "required": ["type"],
                "additionalProperties": false
            }),
        ),
        descriptor(
            "feature_query_bbox",
            "Find features whose AABB intersects [w, s, e, n].",
            json!({
                "type": "object",
                "properties": {
                    "w": { "type": "number" }, "s": { "type": "number" },
                    "e": { "type": "number" }, "n": { "type": "number" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 }
                },
                "required": ["w", "s", "e", "n"],
                "additionalProperties": false
            }),
        ),
        descriptor(
            "feature_nearby",
            "Find features within `radius_m` of (lat, lon), nearest first.",
            json!({
                "type": "object",
                "properties": {
                    "lat": { "type": "number" }, "lon": { "type": "number" },
                    "radius_m": { "type": "number", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 }
                },
                "required": ["lat", "lon", "radius_m"],
                "additionalProperties": false
            }),
        ),
        descriptor(
            "feature_count",
            "Count features by `type` (optional) within an optional bbox.",
            json!({
                "type": "object",
                "properties": {
                    "type": { "type": "string" },
                    "bbox": bbox_schema()
                },
                "additionalProperties": false
            }),
        ),
    ]
}

fn descriptor(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema
    })
}

fn bbox_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "w": { "type": "number" }, "s": { "type": "number" },
            "e": { "type": "number" }, "n": { "type": "number" }
        },
        "required": ["w", "s", "e", "n"],
        "additionalProperties": false
    })
}

// --- Dispatch -----------------------------------------------------------

pub fn dispatch(idx: &Indices, name: &str, args: Value) -> Result<Value> {
    match name {
        "dataset_metadata" => dataset_metadata(idx),
        "catalogue_search" => catalogue_search(idx, args),
        "catalogue_describe_feature" => catalogue_describe_feature(idx, args),
        "catalogue_describe_attribute" => catalogue_describe_attribute(idx, args),
        "feature_get" => feature_get(idx, args),
        "feature_query_by_type" => feature_query_by_type(idx, args),
        "feature_query_bbox" => feature_query_bbox(idx, args),
        "feature_nearby" => feature_nearby(idx, args),
        "feature_count" => feature_count(idx, args),
        other => Err(anyhow!("unknown tool: {}", other)),
    }
}

// --- Tools --------------------------------------------------------------

fn dataset_metadata(idx: &Indices) -> Result<Value> {
    let cell = &idx.cell;
    let extent = idx.geometry.extent().map(|b| {
        json!({
            "w": b.min_lon, "s": b.min_lat,
            "e": b.max_lon, "n": b.max_lat
        })
    });
    let hist: Vec<Value> = idx
        .feature
        .type_histogram()
        .into_iter()
        .map(|(code, count)| json!({ "type": code, "count": count }))
        .collect();
    Ok(json!({
        "dataset": {
            "name": cell.dsid.dataset_name,
            "edition_number": cell.dsid.edition_number,
            "update_number": cell.dsid.update_number,
            "issue_date": cell.dsid.issue_date,
            "compilation_scale": cell.compilation_scale,
            "minimum_display_scale": cell.minimum_display_scale,
            "maximum_display_scale": cell.maximum_display_scale,
            "file": cell.file_path.display().to_string(),
        },
        "extent": extent,
        "feature_count": idx.feature.len(),
        "feature_types": hist,
        "catalogue": {
            "name": idx.fc.name,
            "version": idx.fc.version,
            "product_id": idx.fc.product_id,
            "feature_type_count": idx.catalogue.feature_count(),
            "simple_attribute_count": idx.catalogue.simple_attribute_count(),
        },
        "evidence": [],
    }))
}

fn catalogue_search(idx: &Indices, args: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct A {
        term: String,
        #[serde(default = "default_limit_25")]
        limit: usize,
    }
    let a: A = serde_json::from_value(args)?;
    let hits = idx.catalogue.search(&a.term, a.limit);
    let evidence: Vec<EvidenceRef> = hits
        .iter()
        .map(|h| match h.kind {
            crate::indices::catalogue::EntityKind::Feature => {
                EvidenceRef::catalogue_feature(&h.code)
            }
            _ => EvidenceRef::catalogue_attribute(&h.code),
        })
        .collect();
    Ok(json!({
        "query": a.term,
        "results": hits.into_iter().map(|h| json!({
            "kind": match h.kind {
                crate::indices::catalogue::EntityKind::Feature => "feature_type",
                crate::indices::catalogue::EntityKind::SimpleAttribute => "simple_attribute",
                crate::indices::catalogue::EntityKind::ComplexAttribute => "complex_attribute",
                crate::indices::catalogue::EntityKind::Information => "information_type",
            },
            "code": h.code,
            "name": h.name,
            "definition": h.definition,
        })).collect::<Vec<_>>(),
        "evidence": evidence,
    }))
}

fn catalogue_describe_feature(idx: &Indices, args: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct A {
        code: String,
    }
    let a: A = serde_json::from_value(args)?;
    let Some(ft) = idx.catalogue.describe_feature(&a.code) else {
        return Ok(json!({
            "found": false,
            "code": a.code,
            "message": format!("No feature type '{}' in the catalogue", a.code),
            "evidence": [],
        }));
    };
    Ok(json!({
        "found": true,
        "code": ft.code,
        "name": ft.name,
        "definition": ft.definition,
        "is_abstract": ft.is_abstract,
        "super_type": ft.super_type,
        "permitted_primitives": ft.permitted_primitives.iter().map(|p| format!("{:?}", p)).collect::<Vec<_>>(),
        "attribute_bindings": describe_bindings(ft),
        "feature_bindings": ft.feature_bindings.iter().map(|fb| json!({
            "feature_type_code": fb.feature_type_code,
            "association": fb.association,
            "role": fb.role,
        })).collect::<Vec<_>>(),
        "evidence": [EvidenceRef::catalogue_feature(&ft.code)],
    }))
}

fn describe_bindings(ft: &FeatureType) -> Vec<Value> {
    ft.attribute_bindings
        .iter()
        .map(|b| {
            json!({
                "attribute_code": b.attribute_code,
                "multiplicity_min": b.multiplicity.lower,
                "multiplicity_max": b.multiplicity.upper,
                "sequential": b.sequential,
            })
        })
        .collect()
}

fn catalogue_describe_attribute(idx: &Indices, args: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct A {
        code: String,
    }
    let a: A = serde_json::from_value(args)?;
    if let Some(sa) = idx.catalogue.describe_simple_attribute(&a.code) {
        return Ok(describe_simple_attr(sa));
    }
    if let Some(ca) = idx.catalogue.describe_complex_attribute(&a.code) {
        return Ok(describe_complex_attr(ca));
    }
    Ok(json!({
        "found": false,
        "code": a.code,
        "message": format!("No attribute '{}' in the catalogue", a.code),
        "evidence": [],
    }))
}

fn describe_simple_attr(sa: &SimpleAttribute) -> Value {
    json!({
        "found": true,
        "kind": "simple",
        "code": sa.code,
        "name": sa.name,
        "definition": sa.definition,
        "value_type": format!("{:?}", sa.value_type),
        "uom": sa.uom,
        "listed_values": sa.listed_values.iter().map(|lv| json!({
            "code": lv.code,
            "label": lv.label,
            "definition": lv.definition,
        })).collect::<Vec<_>>(),
        "quantitative_range": sa.quantitative_range.as_ref().map(|q| json!({
            "minimum": q.minimum, "maximum": q.maximum
        })),
        "evidence": [EvidenceRef::catalogue_attribute(&sa.code)],
    })
}

fn describe_complex_attr(ca: &ComplexAttribute) -> Value {
    json!({
        "found": true,
        "kind": "complex",
        "code": ca.code,
        "name": ca.name,
        "definition": ca.definition,
        "sub_attributes": ca.sub_attributes.iter().map(|b| json!({
            "attribute_code": b.attribute_code,
            "multiplicity_min": b.multiplicity.lower,
            "multiplicity_max": b.multiplicity.upper,
        })).collect::<Vec<_>>(),
        "evidence": [EvidenceRef::catalogue_attribute(&ca.code)],
    })
}

fn feature_get(idx: &Indices, args: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct A {
        id: FeatureId,
    }
    let a: A = serde_json::from_value(args)?;
    let Some(f) = idx.cell.features.get(&a.id) else {
        return Ok(json!({
            "found": false,
            "id": a.id,
            "message": format!("No feature with id {} in the loaded cell", a.id),
            "evidence": [],
        }));
    };
    Ok(feature_to_json(idx, a.id, f))
}

fn feature_query_by_type(idx: &Indices, args: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct A {
        #[serde(rename = "type")]
        type_code: String,
        #[serde(default)]
        bbox: Option<BboxArg>,
        #[serde(default = "default_limit_100")]
        limit: usize,
    }
    let a: A = serde_json::from_value(args)?;
    let mut ids: Vec<FeatureId> = idx.feature.ids_by_type(&a.type_code).to_vec();
    if let Some(b) = a.bbox {
        let bbox = Bbox {
            min_lon: b.w,
            min_lat: b.s,
            max_lon: b.e,
            max_lat: b.n,
        };
        ids.retain(|id| {
            idx.geometry
                .get(*id)
                .is_some_and(|g| g.bbox.intersects(&bbox))
        });
    }
    ids.truncate(a.limit);
    Ok(json!({
        "type": a.type_code,
        "matched": ids.len(),
        "features": ids.iter().filter_map(|id| {
            idx.cell.features.get(id).map(|f| feature_summary(idx, *id, f))
        }).collect::<Vec<_>>(),
        "evidence": ids.iter().map(|id| {
            EvidenceRef::feature(*id, idx.cell.features.get(id).and_then(|f| f.feature_code.as_deref()))
        }).collect::<Vec<_>>(),
    }))
}

fn feature_query_bbox(idx: &Indices, args: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct A {
        w: f64,
        s: f64,
        e: f64,
        n: f64,
        #[serde(default = "default_limit_100")]
        limit: usize,
    }
    let a: A = serde_json::from_value(args)?;
    let bbox = Bbox {
        min_lon: a.w,
        min_lat: a.s,
        max_lon: a.e,
        max_lat: a.n,
    };
    let mut ids = idx.geometry.query_bbox(bbox);
    ids.truncate(a.limit);
    Ok(json!({
        "bbox": { "w": a.w, "s": a.s, "e": a.e, "n": a.n },
        "matched": ids.len(),
        "features": ids.iter().filter_map(|id| {
            idx.cell.features.get(id).map(|f| feature_summary(idx, *id, f))
        }).collect::<Vec<_>>(),
        "evidence": ids.iter().map(|id| {
            EvidenceRef::feature(*id, idx.cell.features.get(id).and_then(|f| f.feature_code.as_deref()))
        }).collect::<Vec<_>>(),
    }))
}

fn feature_nearby(idx: &Indices, args: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct A {
        lat: f64,
        lon: f64,
        radius_m: f64,
        #[serde(default = "default_limit_100")]
        limit: usize,
    }
    let a: A = serde_json::from_value(args)?;
    let mut hits = idx.geometry.query_nearby(a.lat, a.lon, a.radius_m);
    hits.truncate(a.limit);
    Ok(json!({
        "center": { "lat": a.lat, "lon": a.lon },
        "radius_m": a.radius_m,
        "matched": hits.len(),
        "features": hits.iter().filter_map(|(id, dist)| {
            let f = idx.cell.features.get(id)?;
            let mut summary = feature_summary(idx, *id, f);
            summary["distance_m"] = json!(*dist);
            Some(summary)
        }).collect::<Vec<_>>(),
        "evidence": hits.iter().map(|(id, _)| {
            EvidenceRef::feature(*id, idx.cell.features.get(id).and_then(|f| f.feature_code.as_deref()))
        }).collect::<Vec<_>>(),
    }))
}

fn feature_count(idx: &Indices, args: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct A {
        #[serde(default)]
        #[serde(rename = "type")]
        type_code: Option<String>,
        #[serde(default)]
        bbox: Option<BboxArg>,
    }
    let a: A = serde_json::from_value(args)?;
    let candidates: Vec<FeatureId> = match &a.type_code {
        Some(code) => idx.feature.ids_by_type(code).to_vec(),
        None => idx.feature.all_ids().collect(),
    };
    let count = match a.bbox {
        Some(b) => {
            let bbox = Bbox {
                min_lon: b.w,
                min_lat: b.s,
                max_lon: b.e,
                max_lat: b.n,
            };
            candidates
                .iter()
                .filter(|id| {
                    idx.geometry
                        .get(**id)
                        .is_some_and(|g| g.bbox.intersects(&bbox))
                })
                .count()
        }
        None => candidates.len(),
    };
    Ok(json!({
        "type": a.type_code,
        "bbox": a.bbox.map(|b| json!({"w": b.w, "s": b.s, "e": b.e, "n": b.n})),
        "count": count,
        "evidence": [],
    }))
}

// --- Helpers ------------------------------------------------------------

#[derive(serde::Deserialize, Clone, Copy)]
struct BboxArg {
    w: f64,
    s: f64,
    e: f64,
    n: f64,
}

fn default_limit_25() -> usize {
    25
}
fn default_limit_100() -> usize {
    100
}

fn feature_summary(idx: &Indices, id: FeatureId, f: &FeatureRecord) -> Value {
    let geom = idx.geometry.get(id);
    let bbox = geom.map(|g| {
        json!({
            "w": g.bbox.min_lon, "s": g.bbox.min_lat,
            "e": g.bbox.max_lon, "n": g.bbox.max_lat
        })
    });
    let centroid = geom.map(|g| json!({ "lon": g.centroid.0, "lat": g.centroid.1 }));
    json!({
        "id": id,
        "type": f.feature_code,
        "primitive": format!("{:?}", f.primitive_type),
        "bbox": bbox,
        "centroid": centroid,
    })
}

fn feature_to_json(idx: &Indices, id: FeatureId, f: &FeatureRecord) -> Value {
    let geom = idx.geometry.get(id);
    let primary_point: Option<Coordinate> = f
        .spatial_associations
        .first()
        .and_then(|s| idx.cell.points.get(&s.spatial_id.key()))
        .map(|p| p.position);

    let attrs: Vec<Value> = f
        .attributes
        .iter()
        .map(|a| {
            let bound = f
                .feature_code
                .as_deref()
                .map(|fc| {
                    crate::indices::evidence::attribute_is_bound(
                        idx,
                        fc,
                        a.code.as_deref().unwrap_or(""),
                    )
                })
                .unwrap_or(false);
            // Prefer the FC-resolved typed value (handles enumeration
            // label lookup); fall back to raw atvl so the JSON never
            // shows an empty value when the cell did encode something.
            let rendered = format_attribute_value(&a.value);
            let value_str = if rendered.is_empty() && !a.atvl.is_empty() {
                a.atvl.clone()
            } else {
                rendered
            };
            json!({
                "code": a.code,
                "value": value_str,
                "is_bound_in_catalogue": bound,
            })
        })
        .collect();

    let mut evidence = vec![EvidenceRef::feature(id, f.feature_code.as_deref())];
    if let Some(code) = f.feature_code.as_deref() {
        if catalogue_definition_for_feature(idx, f).is_some() {
            evidence.push(EvidenceRef::catalogue_feature(code));
        }
    }

    json!({
        "found": true,
        "id": id,
        "type": f.feature_code,
        "primitive": format!("{:?}", f.primitive_type),
        "bbox": geom.map(|g| json!({
            "w": g.bbox.min_lon, "s": g.bbox.min_lat,
            "e": g.bbox.max_lon, "n": g.bbox.max_lat
        })),
        "centroid": geom.map(|g| json!({ "lon": g.centroid.0, "lat": g.centroid.1 })),
        "primary_point": primary_point.map(|p| json!({ "lon": p.x, "lat": p.y })),
        "attributes": attrs,
        "catalogue_definition": catalogue_definition_for_feature(idx, f),
        "spatial_association_count": f.spatial_associations.len(),
        "evidence": evidence,
    })
}

fn format_attribute_value(v: &Option<AttributeValue>) -> String {
    match v {
        Some(av) => format!("{}", av),
        None => String::new(),
    }
}
