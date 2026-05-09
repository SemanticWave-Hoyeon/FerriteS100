//! Feature Catalogue search index.
//!
//! Plan2 §4 distinguishes "what does this feature/attribute mean?" (catalogue)
//! from "what is at this position?" (geometry). The catalogue tools answer
//! the first kind purely from the FC XML, with no reference to the loaded
//! cell — so they answer the same way whether or not a chart is loaded.
//!
//! The match strategy is a deliberately simple lowercased substring search
//! across `code | name | definition`. The corpus is small (hundreds of items)
//! so no inverted index is needed, and "give me everything mentioning
//! 'buoy'" is exactly what plan2 §6 §M5 wants from `catalogue_search`.

use rustc_hash::FxHashMap;

use ferrite_feature_catalog::{ComplexAttribute, FeatureCatalogue, FeatureType, SimpleAttribute};

#[derive(Debug, Clone, Copy)]
pub enum EntityKind {
    Feature,
    SimpleAttribute,
    ComplexAttribute,
    Information,
}

#[derive(Debug, Clone)]
pub struct CatalogueHit {
    pub kind: EntityKind,
    pub code: String,
    pub name: String,
    pub definition: Option<String>,
}

pub struct CatalogueIndex {
    /// Pre-lowercased haystack lines. Each entry is
    /// `(haystack, hit)` — the haystack is `"{code} {name} {definition}"`.
    haystack: Vec<(String, CatalogueHit)>,
    /// Direct lookup tables for `*_describe_*` tools.
    feature_by_code: FxHashMap<String, FeatureType>,
    simple_by_code: FxHashMap<String, SimpleAttribute>,
    complex_by_code: FxHashMap<String, ComplexAttribute>,
}

impl CatalogueIndex {
    pub fn build(fc: &FeatureCatalogue) -> Self {
        let mut haystack = Vec::with_capacity(
            fc.feature_types.len()
                + fc.simple_attributes.len()
                + fc.complex_attributes.len()
                + fc.information_types.len(),
        );

        let push = |hs: &mut Vec<(String, CatalogueHit)>, hit: CatalogueHit| {
            let body = format!(
                "{} {} {}",
                hit.code,
                hit.name,
                hit.definition.as_deref().unwrap_or("")
            )
            .to_lowercase();
            hs.push((body, hit));
        };

        for ft in fc.feature_types.values() {
            push(
                &mut haystack,
                CatalogueHit {
                    kind: EntityKind::Feature,
                    code: ft.code.clone(),
                    name: ft.name.clone(),
                    definition: ft.definition.clone(),
                },
            );
        }
        for sa in fc.simple_attributes.values() {
            push(
                &mut haystack,
                CatalogueHit {
                    kind: EntityKind::SimpleAttribute,
                    code: sa.code.clone(),
                    name: sa.name.clone(),
                    definition: sa.definition.clone(),
                },
            );
        }
        for ca in fc.complex_attributes.values() {
            push(
                &mut haystack,
                CatalogueHit {
                    kind: EntityKind::ComplexAttribute,
                    code: ca.code.clone(),
                    name: ca.name.clone(),
                    definition: ca.definition.clone(),
                },
            );
        }
        for it in fc.information_types.values() {
            push(
                &mut haystack,
                CatalogueHit {
                    kind: EntityKind::Information,
                    code: it.code.clone(),
                    name: it.name.clone(),
                    definition: it.definition.clone(),
                },
            );
        }

        let feature_by_code = fc
            .feature_types
            .values()
            .map(|f| (f.code.clone(), f.clone()))
            .collect();
        let simple_by_code = fc
            .simple_attributes
            .values()
            .map(|a| (a.code.clone(), a.clone()))
            .collect();
        let complex_by_code = fc
            .complex_attributes
            .values()
            .map(|a| (a.code.clone(), a.clone()))
            .collect();

        CatalogueIndex {
            haystack,
            feature_by_code,
            simple_by_code,
            complex_by_code,
        }
    }

    pub fn feature_count(&self) -> usize {
        self.feature_by_code.len()
    }

    pub fn simple_attribute_count(&self) -> usize {
        self.simple_by_code.len()
    }

    /// Substring search across catalogue entries.
    /// Empty query returns nothing — the 9-tool surface uses
    /// `dataset_metadata` for "what types are present?" instead.
    pub fn search(&self, query: &str, limit: usize) -> Vec<CatalogueHit> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        self.haystack
            .iter()
            .filter(|(body, _)| body.contains(&q))
            .take(limit)
            .map(|(_, hit)| hit.clone())
            .collect()
    }

    pub fn describe_feature(&self, code: &str) -> Option<&FeatureType> {
        // Try exact, then case-insensitive
        self.feature_by_code.get(code).or_else(|| {
            self.feature_by_code
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(code))
                .map(|(_, v)| v)
        })
    }

    pub fn describe_simple_attribute(&self, code: &str) -> Option<&SimpleAttribute> {
        self.simple_by_code.get(code).or_else(|| {
            self.simple_by_code
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(code))
                .map(|(_, v)| v)
        })
    }

    pub fn describe_complex_attribute(&self, code: &str) -> Option<&ComplexAttribute> {
        self.complex_by_code.get(code).or_else(|| {
            self.complex_by_code
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(code))
                .map(|(_, v)| v)
        })
    }
}
