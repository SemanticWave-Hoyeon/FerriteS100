//! Conservative owned allocation-payload accounting, not allocator overhead or RSS.
use crate::*;
use std::{collections::HashMap, mem::size_of};
fn add(a: usize, b: usize) -> Option<usize> {
    a.checked_add(b)
}
fn vec_bytes<T>(v: &Vec<T>) -> Option<usize> {
    v.capacity().checked_mul(size_of::<T>())
}
// std's current power-of-two hash table has at most 2*(capacity+1) buckets.
// Charge each full tuple bucket plus control bytes and alignment tail conservatively.
// Requalify this bound when the std HashMap implementation changes.
fn map_bytes<K, V>(v: &HashMap<K, V>) -> Option<usize> {
    if v.capacity() == 0 {
        return Some(0);
    }
    let buckets = v.capacity().checked_add(1)?.checked_mul(2)?;
    add(
        buckets.checked_mul(size_of::<(K, V)>().checked_add(1)?)?,
        64,
    )
}
fn value(v: &AttributeValue, depth: usize) -> Option<usize> {
    if depth > 64 {
        return None;
    }
    match v {
        AttributeValue::Text(s)
        | AttributeValue::Date(s)
        | AttributeValue::Time(s)
        | AttributeValue::DateTime(s)
        | AttributeValue::Enumeration(_, s) => Some(s.capacity()),
        AttributeValue::List(v) => v
            .iter()
            .try_fold(vec_bytes(v)?, |n, v| add(n, value(v, depth + 1)?)),
        _ => Some(0),
    }
}
fn attrs(v: &Vec<Attribute>) -> Option<usize> {
    v.iter().try_fold(vec_bytes(v)?, |n, a| {
        let n = add(n, a.atvl.capacity())?;
        let n = add(n, a.code.as_ref().map_or(0, String::capacity))?;
        add(
            n,
            match &a.value {
                Some(v) => value(v, 0)?,
                None => 0,
            },
        )
    })
}
fn infos(v: &Vec<InformationAssociation>) -> Option<usize> {
    v.iter()
        .try_fold(vec_bytes(v)?, |n, a| add(n, attrs(&a.attributes)?))
}
fn codes(m: &CodeMapping) -> Option<usize> {
    let n = m
        .num_to_str
        .values()
        .try_fold(map_bytes(&m.num_to_str)?, |n, s| add(n, s.capacity()))?;
    m.str_to_num
        .keys()
        .try_fold(add(n, map_bytes(&m.str_to_num)?)?, |n, s| {
            add(n, s.capacity())
        })
}
impl S101Cell {
    /// All owned String/Vec capacities and conservative HashMap payload bound.
    /// Excludes the worker's mutable copy, allocator metadata, and process RSS.
    pub fn retained_payload_upper_bound(&self) -> Option<usize> {
        let d = &self.dsid;
        let mut n = add(size_of::<Self>(), self.file_path.capacity())?;
        for s in [
            &d.dataset_name,
            &d.dataset_title,
            &d.product_identifier,
            &d.product_edition,
            &d.application_profile,
            &d.update_application_date,
            &d.issue_date,
        ] {
            n = add(n, s.capacity())?
        }
        let c = &self.code_mappings;
        for m in [
            &c.attributes,
            &c.information_types,
            &c.feature_types,
            &c.information_associations,
            &c.feature_associations,
            &c.association_roles,
        ] {
            n = add(n, codes(m)?)?
        }
        n = add(n, map_bytes(&self.points)?)?;
        n = self
            .multi_points
            .values()
            .try_fold(add(n, map_bytes(&self.multi_points)?)?, |n, p| {
                add(n, vec_bytes(&p.positions)?)
            })?;
        n = self
            .curves
            .values()
            .try_fold(add(n, map_bytes(&self.curves)?)?, |n, c| {
                c.segments
                    .iter()
                    .try_fold(add(n, vec_bytes(&c.segments)?)?, |n, s| {
                        add(n, vec_bytes(&s.positions)?)
                    })
            })?;
        n = self
            .composite_curves
            .values()
            .try_fold(add(n, map_bytes(&self.composite_curves)?)?, |n, c| {
                add(n, vec_bytes(&c.curves)?)
            })?;
        n = self
            .surfaces
            .values()
            .try_fold(add(n, map_bytes(&self.surfaces)?)?, |n, s| {
                s.interior_rings.iter().try_fold(
                    add(
                        add(n, vec_bytes(&s.exterior_ring)?)?,
                        vec_bytes(&s.interior_rings)?,
                    )?,
                    |n, r| add(n, vec_bytes(r)?),
                )
            })?;
        n = self
            .features
            .values()
            .try_fold(add(n, map_bytes(&self.features)?)?, |n, f| {
                let n = add(n, attrs(&f.attributes)?)?;
                let n = add(n, vec_bytes(&f.spatial_associations)?)?;
                let n = add(n, infos(&f.information_associations)?)?;
                let n = f
                    .feature_associations
                    .iter()
                    .try_fold(add(n, vec_bytes(&f.feature_associations)?)?, |n, a| {
                        add(n, attrs(&a.attributes)?)
                    })?;
                add(
                    add(n, vec_bytes(&f.masks)?)?,
                    f.feature_code.as_ref().map_or(0, String::capacity),
                )
            })?;
        n = self.information.values().try_fold(
            add(n, map_bytes(&self.information)?)?,
            |n, i| {
                add(
                    add(
                        add(n, attrs(&i.attributes)?)?,
                        infos(&i.information_associations)?,
                    )?,
                    i.info_code.as_ref().map_or(0, String::capacity),
                )
            },
        )?;
        self.spatial_information_associations.values().try_fold(
            add(n, map_bytes(&self.spatial_information_associations)?)?,
            |n, a| add(n, infos(a)?),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn empty() -> S101Cell {
        S101Cell {
            file_path: "x".into(),
            dsid: DatasetIdentification::default(),
            code_mappings: DatasetCodeMappings::default(),
            coord_factor: 1.0,
            coord_factor_y: 1.0,
            coord_factor_z: 1.0,
            coord_origin_x: 0.0,
            coord_origin_y: 0.0,
            coord_origin_z: 0.0,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: HashMap::new(),
            multi_points: HashMap::new(),
            curves: HashMap::new(),
            composite_curves: HashMap::new(),
            surfaces: HashMap::new(),
            features: HashMap::new(),
            information: HashMap::new(),
            spatial_information_associations: HashMap::new(),
        }
    }

    #[test]
    fn nested_owned_capacity_is_charged() {
        let mut c = empty();
        let before = c.retained_payload_upper_bound().unwrap();
        c.multi_points.insert(
            1,
            MultiPointRecord {
                id: RecordId::new(110, 1),
                positions: Vec::with_capacity(100),
                update_instruction: 1,
            },
        );
        assert!(
            c.retained_payload_upper_bound().unwrap() >= before + 100 * size_of::<Coordinate>()
        );
    }
    #[test]
    fn deep_values_decline_without_unbounded_recursion() {
        let mut v = AttributeValue::Text("a".into());
        for _ in 0..66 {
            v = AttributeValue::List(vec![v])
        }
        assert!(value(&v, 0).is_none());
    }
    #[test]
    fn raw_clone_owner_resolution_is_independent() {
        let mut raw = empty();
        raw.features.insert(
            1,
            FeatureRecord {
                frid: FRID {
                    rcid: 1,
                    nftc: 1,
                    rver: 1,
                    ruin: 1,
                },
                foid: None,
                attributes: vec![Attribute {
                    natc: 1,
                    atix: 1,
                    paix: 0,
                    atvl: "7.5".into(),
                    value: None,
                    code: None,
                }],
                spatial_associations: vec![],
                information_associations: vec![],
                feature_associations: vec![],
                masks: vec![],
                feature_code: None,
                primitive_type: SpatialPrimitiveType::NoGeometry,
            },
        );
        let mut first = raw.clone();
        let a = &mut first.features.get_mut(&1).unwrap().attributes[0];
        a.code = Some("ownerA".into());
        a.value = Some(AttributeValue::Real(7.5));
        let second = raw.clone();
        assert!(second.features[&1].attributes[0].value.is_none());
        assert!(second.features[&1].attributes[0].code.is_none());
        assert_eq!(second.features[&1].attributes[0].atvl, "7.5");
    }
}
