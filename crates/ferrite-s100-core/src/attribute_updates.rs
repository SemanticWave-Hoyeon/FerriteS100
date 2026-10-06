//! Ordered Part 10a attribute updates. Parent ordinals are field-local;
//! arena handles survive sibling index shifts and never alias deleted nodes.
use crate::{Result, S100Error};
use ferrite_iso8211::{RawField, FIELD_TERMINATOR};
use std::collections::BTreeMap;
fn bad(message: &str) -> S100Error {
    S100Error::InvalidRecord(message.into())
}
fn check(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(bad(message))
    }
}
#[derive(Default)]
struct Node {
    code: u16,
    value: Vec<u8>,
    alive: bool,
    children: BTreeMap<u16, Vec<usize>>,
    order: Vec<u16>,
}
struct Tuple<'a> {
    code: u16,
    index: u16,
    parent: u16,
    op: u8,
    value: &'a [u8],
}
fn tuples(data: &[u8], max: usize) -> Result<Vec<Tuple<'_>>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < data.len() {
        check(out.len() < max, "Attribute tuple budget exceeded")?;
        check(data.len() - p >= 8, "Truncated attribute tuple")?;
        let code = u16::from_le_bytes(data[p..p + 2].try_into().unwrap());
        let index = u16::from_le_bytes(data[p + 2..p + 4].try_into().unwrap());
        let parent = u16::from_le_bytes(data[p + 4..p + 6].try_into().unwrap());
        let op = data[p + 6];
        p += 7;
        let len = data[p..]
            .iter()
            .position(|b| *b == 0x1f)
            .ok_or_else(|| bad("Unterminated attribute value"))?;
        let value = &data[p..p + len];
        p += len + 1;
        check(
            code > 0 && index > 0 && matches!(op, 1..=3),
            "Invalid attribute identity/instruction",
        )?;
        check(
            !value.contains(&0) && std::str::from_utf8(value).is_ok(),
            "Invalid attribute UTF-8 value",
        )?;
        check(
            op != 2 || value.is_empty(),
            "Deleted attribute value must be unknown",
        )?;
        out.push(Tuple {
            code,
            index,
            parent,
            op,
            value,
        });
    }
    Ok(out)
}
struct Tree {
    nodes: Vec<Node>,
    max: usize,
}
impl Tree {
    fn new(max: usize) -> Self {
        Self {
            nodes: vec![Node {
                alive: true,
                ..Node::default()
            }],
            max,
        }
    }
    fn field(&mut self, data: &[u8], base: bool) -> Result<()> {
        let tuples = tuples(data, self.max)?;
        let mut handles: Vec<usize> = Vec::new();
        let mut ancestors: Vec<usize> = Vec::new();
        for (ordinal, t) in tuples.iter().enumerate() {
            let parent = if t.parent == 0 {
                ancestors.clear();
                0
            } else {
                let position = usize::from(t.parent) - 1;
                check(position < ordinal, "Attribute parent must precede child")?;
                let at = ancestors
                    .iter()
                    .position(|i| *i == position)
                    .ok_or_else(|| bad("Attribute parent violates preorder"))?;
                ancestors.truncate(at + 1);
                let id = handles[position];
                check(self.nodes[id].alive, "Attribute parent was deleted")?;
                id
            };
            check(
                !base || t.op == 1,
                "Base attributes must use insertion instructions",
            )?;
            let existing = self.nodes[parent]
                .children
                .get(&t.code)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let index = usize::from(t.index) - 1;
            let id = match t.op {
                1 => {
                    check(
                        index <= existing.len(),
                        "Attribute insertion index has a gap",
                    )?;
                    check(
                        !base || index == existing.len(),
                        "Duplicate or unordered base attribute identity",
                    )?;
                    check(
                        self.nodes.len() - 1 < self.max,
                        "Retained attribute identity budget exceeded",
                    )?;
                    let id = self.nodes.len();
                    self.nodes.push(Node {
                        code: t.code,
                        value: t.value.to_vec(),
                        alive: true,
                        ..Node::default()
                    });
                    let node = &mut self.nodes[parent];
                    if !node.children.contains_key(&t.code) {
                        node.order.push(t.code);
                    }
                    node.children.entry(t.code).or_default().insert(index, id);
                    id
                }
                2 | 3 => {
                    let id = *existing
                        .get(index)
                        .ok_or_else(|| bad("Attribute update target missing"))?;
                    if t.op == 3 {
                        self.nodes[id].value = t.value.to_vec();
                    } else {
                        self.nodes[parent]
                            .children
                            .get_mut(&t.code)
                            .unwrap()
                            .remove(index);
                        let mut pending = vec![id];
                        while let Some(n) = pending.pop() {
                            check(self.nodes[n].alive, "Duplicate deleted attribute node")?;
                            self.nodes[n].alive = false;
                            pending.extend(self.nodes[n].children.values().flatten().copied());
                            self.nodes[n].value.clear();
                            self.nodes[n].children.clear();
                            self.nodes[n].order.clear();
                        }
                    }
                    id
                }
                _ => unreachable!(),
            };
            // Field-local tuple references resolve to stable arena handles, even after
            // insertion changes sibling indices. A later reference to a delete fails.
            handles.push(id);
            ancestors.push(ordinal);
        }
        Ok(())
    }
    fn encode(&self, max_bytes: usize) -> Result<RawField> {
        let mut data = Vec::new();
        let mut ordinal = 0usize;
        let mut pending = Vec::<(usize, u16, u16)>::new();
        let push_children =
            |node: usize, parent: u16, pending: &mut Vec<(usize, u16, u16)>| -> Result<()> {
                for code in self.nodes[node].order.iter().rev() {
                    if let Some(ids) = self.nodes[node].children.get(code) {
                        for (i, id) in ids.iter().enumerate().rev() {
                            let ix = u16::try_from(i + 1)
                                .map_err(|_| bad("Attribute sibling index overflow"))?;
                            pending.push((*id, ix, parent));
                        }
                    }
                }
                Ok(())
            };
        push_children(0, 0, &mut pending)?;
        while let Some((id, index, parent)) = pending.pop() {
            ordinal += 1;
            let n = &self.nodes[id];
            let ordinal16 = if n.children.values().any(|v| !v.is_empty()) {
                u16::try_from(ordinal).map_err(|_| bad("Attribute parent ordinal overflow"))?
            } else {
                0
            };
            check(n.alive, "Deleted node reached attribute output")?;
            check(
                data.len()
                    .checked_add(8 + n.value.len())
                    .and_then(|v| v.checked_add(1))
                    .is_some_and(|v| v <= max_bytes),
                "Attribute encoded byte budget exceeded",
            )?;
            data.extend_from_slice(&n.code.to_le_bytes());
            data.extend_from_slice(&index.to_le_bytes());
            data.extend_from_slice(&parent.to_le_bytes());
            data.push(1);
            data.extend_from_slice(&n.value);
            data.push(0x1f);
            push_children(id, ordinal16, &mut pending)?;
        }
        check(
            data.len() < max_bytes,
            "Attribute field byte budget exceeded",
        )?;
        data.push(FIELD_TERMINATOR);
        Ok(RawField::new("ATTR".into(), data))
    }
}
/// Pure materialization: a failed command publishes no output and leaves input
/// fields intact. Repeated input ATTR fields keep separate PAIX namespaces.
pub(crate) fn apply_attributes(
    old: &[RawField],
    updates: &[RawField],
    max_items: usize,
    max_bytes: usize,
) -> Result<RawField> {
    let mut command_count = 0usize;
    for f in updates.iter().filter(|f| f.tag == "ATTR") {
        command_count = command_count
            .checked_add(tuples(f.data_trimmed(), max_items)?.len())
            .ok_or_else(|| bad("Attribute command count overflow"))?;
        check(
            command_count <= max_items,
            "Aggregate attribute command budget exceeded",
        )?;
    }
    let mut tree = Tree::new(max_items);
    for f in old.iter().filter(|f| f.tag == "ATTR") {
        tree.field(f.data_trimmed(), true)?;
    }
    for f in updates.iter().filter(|f| f.tag == "ATTR") {
        tree.field(f.data_trimmed(), false)?;
    }
    tree.encode(max_bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn attr(rows: &[(u16, u16, u16, u8, &str)]) -> RawField {
        let mut d = Vec::new();
        for &(c, i, p, o, v) in rows {
            d.extend(c.to_le_bytes());
            d.extend(i.to_le_bytes());
            d.extend(p.to_le_bytes());
            d.push(o);
            d.extend(v.as_bytes());
            d.push(0x1f);
        }
        d.push(FIELD_TERMINATOR);
        RawField::new("ATTR".into(), d)
    }
    fn rows(f: &RawField) -> Vec<(u16, u16, u16, u8, String)> {
        tuples(f.data_trimmed(), 100)
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t.code,
                    t.index,
                    t.parent,
                    t.op,
                    std::str::from_utf8(t.value).unwrap().into(),
                )
            })
            .collect()
    }
    #[test]
    fn ordered_insert_modify_preserve_unmentioned_and_empty_is_unknown() {
        let base = attr(&[
            (22, 1, 0, 1, ""),
            (26, 1, 1, 1, ""),
            (29, 1, 2, 1, "17"),
            (29, 2, 2, 1, "43"),
            (21, 1, 0, 1, "Vachon"),
        ]);
        let update = attr(&[
            (22, 1, 0, 3, ""),
            (26, 1, 1, 3, ""),
            (29, 2, 2, 1, "32"),
            (29, 3, 2, 3, "7"),
            (21, 1, 0, 3, ""),
        ]);
        let out = apply_attributes(&[base], &[update], 100, 1000).unwrap();
        assert_eq!(
            rows(&out),
            vec![
                (22, 1, 0, 1, "".into()),
                (26, 1, 1, 1, "".into()),
                (29, 1, 2, 1, "17".into()),
                (29, 2, 2, 1, "32".into()),
                (29, 3, 2, 1, "7".into()),
                (21, 1, 0, 1, "".into())
            ]
        );
    }
    #[test]
    fn delete_complex_shifts_indices_and_removes_descendants() {
        let base = attr(&[
            (22, 1, 0, 1, ""),
            (25, 1, 1, 1, "42"),
            (22, 2, 0, 1, ""),
            (25, 1, 3, 1, "88"),
        ]);
        let update = attr(&[(22, 1, 0, 2, ""), (22, 1, 0, 3, ""), (25, 1, 2, 3, "99")]);
        let out = apply_attributes(&[base], &[update], 100, 1000).unwrap();
        assert_eq!(
            rows(&out),
            vec![(22, 1, 0, 1, "".into()), (25, 1, 1, 1, "99".into())]
        );
    }
    #[test]
    fn repeated_fields_have_local_parents_and_preserve_other_children() {
        let a = attr(&[(3, 1, 0, 1, ""), (4, 1, 1, 1, "eng"), (5, 1, 1, 1, "Old")]);
        let b = attr(&[
            (3, 2, 0, 1, ""),
            (4, 1, 1, 1, "fra"),
            (5, 1, 1, 1, "Ancien"),
        ]);
        let updates = [
            attr(&[(3, 1, 0, 3, ""), (5, 1, 1, 3, "New")]),
            attr(&[(3, 2, 0, 3, ""), (5, 1, 1, 3, "Nouveau")]),
        ];
        let out = apply_attributes(&[a, b], &updates, 100, 1000).unwrap();
        assert_eq!(
            rows(&out),
            vec![
                (3, 1, 0, 1, "".into()),
                (4, 1, 1, 1, "eng".into()),
                (5, 1, 1, 1, "New".into()),
                (3, 2, 0, 1, "".into()),
                (4, 1, 4, 1, "fra".into()),
                (5, 1, 4, 1, "Nouveau".into())
            ]
        );
    }
    #[test]
    fn malformed_preorder_and_deleted_parent_fail_without_output() {
        let base = attr(&[(3, 1, 0, 1, ""), (4, 1, 1, 1, "eng")]);
        let copy = base.data.clone();
        for update in [
            attr(&[(3, 1, 0, 2, ""), (4, 1, 1, 3, "x")]),
            attr(&[(3, 1, 0, 3, ""), (9, 1, 0, 1, ""), (4, 1, 1, 3, "x")]),
            attr(&[(4, 1, 2, 3, "x")]),
            attr(&[(4, 3, 0, 1, "gap")]),
        ] {
            assert!(apply_attributes(std::slice::from_ref(&base), &[update], 100, 1000).is_err());
            assert_eq!(base.data, copy);
        }
        assert!(apply_attributes(std::slice::from_ref(&base), &[], 1, 1000).is_err());
        assert!(apply_attributes(&[base], &[], 100, 8).is_err());
    }
    fn paths(field: &RawField) -> Vec<(Vec<(u16, u16)>, String)> {
        let ts = tuples(field.data_trimmed(), 100).unwrap();
        let mut result: Vec<(Vec<(u16, u16)>, String)> = Vec::new();
        for t in ts {
            let mut p = if t.parent == 0 {
                Vec::new()
            } else {
                result[usize::from(t.parent) - 1].0.clone()
            };
            p.push((t.code, t.index));
            result.push((p, String::from_utf8(t.value.to_vec()).unwrap()));
        }
        result.sort();
        result
    }
    #[test]
    fn primary_oracle_official_figure10a4() {
        let base = [attr(&[
            (21, 1, 0, 1, "Vachon"),
            (22, 1, 0, 1, ""),
            (25, 1, 2, 1, "42.0"),
            (26, 1, 2, 1, ""),
            (29, 1, 4, 1, "17"),
            (29, 2, 4, 1, "43"),
            (23, 1, 0, 1, "12"),
            (24, 1, 0, 1, ""),
            (27, 1, 8, 1, "123"),
            (28, 1, 8, 1, "Canada"),
        ])];
        let updates = [attr(&[
            (22, 1, 0, 3, ""),
            (26, 1, 1, 3, ""),
            (29, 2, 2, 1, "32"),
            (29, 3, 2, 3, "7"),
            (35, 1, 2, 1, ""),
            (36, 1, 5, 1, "22"),
            (37, 1, 5, 1, "123"),
            (32, 1, 0, 1, "abc"),
            (23, 1, 0, 2, ""),
            (24, 1, 0, 3, ""),
            (28, 1, 10, 3, "Germany"),
        ])];
        let out = apply_attributes(&base, &updates, 100, 4000).unwrap();
        let mut expected: Vec<(Vec<(u16, u16)>, String)> = vec![
            (vec![(21, 1)], "Vachon".into()),
            (vec![(22, 1)], "".into()),
            (vec![(22, 1), (25, 1)], "42.0".into()),
            (vec![(22, 1), (26, 1)], "".into()),
            (vec![(22, 1), (26, 1), (29, 1)], "17".into()),
            (vec![(22, 1), (26, 1), (29, 2)], "32".into()),
            (vec![(22, 1), (26, 1), (29, 3)], "7".into()),
            (vec![(22, 1), (26, 1), (35, 1)], "".into()),
            (vec![(22, 1), (26, 1), (35, 1), (36, 1)], "22".into()),
            (vec![(22, 1), (26, 1), (35, 1), (37, 1)], "123".into()),
            (vec![(32, 1)], "abc".into()),
            (vec![(24, 1)], "".into()),
            (vec![(24, 1), (27, 1)], "123".into()),
            (vec![(24, 1), (28, 1)], "Germany".into()),
        ];
        expected.sort();
        assert_eq!(paths(&out), expected);
        assert_eq!(rows(&out).len(), 14);
    }
    #[test]
    fn primary_oracle_delete_complex_root() {
        let base = [attr(&[
            (21, 1, 0, 1, "Vachon"),
            (22, 1, 0, 1, ""),
            (25, 1, 2, 1, "42.0"),
            (26, 1, 2, 1, ""),
            (29, 1, 4, 1, "17"),
            (29, 2, 4, 1, "43"),
            (23, 1, 0, 1, "12"),
            (24, 1, 0, 1, ""),
            (27, 1, 8, 1, "123"),
            (28, 1, 8, 1, "Canada"),
        ])];
        let updates = [attr(&[(22, 1, 0, 2, "")])];
        let out = apply_attributes(&base, &updates, 100, 4000).unwrap();
        let mut expected: Vec<(Vec<(u16, u16)>, String)> = vec![
            (vec![(21, 1)], "Vachon".into()),
            (vec![(23, 1)], "12".into()),
            (vec![(24, 1)], "".into()),
            (vec![(24, 1), (27, 1)], "123".into()),
            (vec![(24, 1), (28, 1)], "Canada".into()),
        ];
        expected.sort();
        assert_eq!(paths(&out), expected);
        assert_eq!(rows(&out).len(), 5);
    }
    #[test]
    fn primary_oracle_empty_modify_is_unknown() {
        let base = [attr(&[
            (21, 1, 0, 1, "Vachon"),
            (22, 1, 0, 1, ""),
            (25, 1, 2, 1, "42.0"),
            (26, 1, 2, 1, ""),
            (29, 1, 4, 1, "17"),
            (29, 2, 4, 1, "43"),
            (23, 1, 0, 1, "12"),
            (24, 1, 0, 1, ""),
            (27, 1, 8, 1, "123"),
            (28, 1, 8, 1, "Canada"),
        ])];
        let updates = [attr(&[(21, 1, 0, 3, "")])];
        let out = apply_attributes(&base, &updates, 100, 4000).unwrap();
        let mut expected: Vec<(Vec<(u16, u16)>, String)> = vec![
            (vec![(21, 1)], "".into()),
            (vec![(22, 1)], "".into()),
            (vec![(22, 1), (25, 1)], "42.0".into()),
            (vec![(22, 1), (26, 1)], "".into()),
            (vec![(22, 1), (26, 1), (29, 1)], "17".into()),
            (vec![(22, 1), (26, 1), (29, 2)], "43".into()),
            (vec![(23, 1)], "12".into()),
            (vec![(24, 1)], "".into()),
            (vec![(24, 1), (27, 1)], "123".into()),
            (vec![(24, 1), (28, 1)], "Canada".into()),
        ];
        expected.sort();
        assert_eq!(paths(&out), expected);
        assert_eq!(rows(&out).len(), 10);
    }
    #[test]
    fn primary_oracle_field_local_parent_ordinals() {
        let base = [
            attr(&[(3, 1, 0, 1, ""), (4, 1, 1, 1, "eng"), (5, 1, 1, 1, "Old")]),
            attr(&[
                (3, 2, 0, 1, ""),
                (4, 1, 1, 1, "fra"),
                (5, 1, 1, 1, "Ancien"),
            ]),
        ];
        let updates = [
            attr(&[(3, 1, 0, 3, ""), (5, 1, 1, 3, "New")]),
            attr(&[(3, 2, 0, 3, ""), (5, 1, 1, 3, "Nouveau")]),
        ];
        let out = apply_attributes(&base, &updates, 100, 4000).unwrap();
        let mut expected: Vec<(Vec<(u16, u16)>, String)> = vec![
            (vec![(3, 1)], "".into()),
            (vec![(3, 1), (4, 1)], "eng".into()),
            (vec![(3, 1), (5, 1)], "New".into()),
            (vec![(3, 2)], "".into()),
            (vec![(3, 2), (4, 1)], "fra".into()),
            (vec![(3, 2), (5, 1)], "Nouveau".into()),
        ];
        expected.sort();
        assert_eq!(paths(&out), expected);
        assert_eq!(rows(&out).len(), 6);
    }
    #[test]
    fn duplicate_base_path_and_aggregate_command_budget_reject() {
        let a = attr(&[(3, 1, 0, 1, "")]);
        let b = attr(&[(3, 1, 0, 1, "")]);
        assert!(apply_attributes(&[a, b], &[], 100, 1000).is_err());
        let base = attr(&[(3, 1, 0, 1, "")]);
        let edits = [attr(&[(3, 1, 0, 3, "a")]), attr(&[(3, 1, 0, 3, "b")])];
        assert!(apply_attributes(&[base], &edits, 1, 1000).is_err());
    }
}
