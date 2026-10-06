//! S-100 9-11.2.2 / 9a-11.2.2.1 drawing-command dependencies.
//! Product adapters provide display-list namespaces, never feature-local IDs.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawingDependency {
    /// Opaque display-list namespace, not a feature ID. Product adapters choose
    /// a namespace to avoid collisions between independently portrayed inputs.
    pub namespace: u64,
    pub id: Option<Box<str>>,
    pub parent_id: Option<Box<str>>,
    /// Retained for backend policy. OEM hover support is optional in S-100.
    pub hover: bool,
}
impl DrawingDependency {
    pub fn new(
        namespace: u64,
        id: Option<&str>,
        parent_id: Option<&str>,
        hover: bool,
    ) -> Option<Self> {
        let id = id.filter(|v| !v.is_empty()).map(Into::into);
        let parent_id = parent_id.filter(|v| !v.is_empty()).map(Into::into);
        (id.is_some() || parent_id.is_some() || hover).then_some(Self {
            namespace,
            id,
            parent_id,
            hover,
        })
    }
}

/// Graph grouped by instruction ID. Multiple commands may share an ID: any
/// executed parent satisfies the dependency. Storing every parent-child pair
/// would be quadratic for repeated IDs; each group is expanded at most once.
#[derive(Debug, Default)]
pub struct DrawingDependencyGraph {
    own_group: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    missing_parent: Vec<bool>,
    declared_parents: Vec<bool>,
    has_parents: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyResolution {
    pub executed: Vec<bool>,
    pub missing_parent_count: usize,
    /// Eligible commands which cannot be grounded in an executed root. This
    /// includes dependency cycles; it is not a claim that every item is cyclic.
    pub ungrounded_count: usize,
}
impl DrawingDependencyGraph {
    pub fn compile<'a>(metadata: impl IntoIterator<Item = Option<&'a DrawingDependency>>) -> Self {
        let meta: Vec<_> = metadata.into_iter().collect();
        let mut groups = HashMap::new();
        for m in meta.iter().flatten() {
            if let Some(id) = m.id.as_deref() {
                let next = groups.len();
                groups.entry((m.namespace, id)).or_insert(next);
            }
        }
        let mut g = Self {
            has_parents: meta.iter().flatten().any(|m| m.parent_id.is_some()),
            own_group: Vec::with_capacity(meta.len()),
            children: vec![Vec::new(); groups.len()],
            missing_parent: Vec::with_capacity(meta.len()),
            declared_parents: Vec::with_capacity(meta.len()),
        };
        for (i, m) in meta.into_iter().enumerate() {
            let parent = m.and_then(|m| m.parent_id.as_deref().map(|p| (m.namespace, p)));
            let group = parent.and_then(|key| groups.get(&key).copied());
            g.declared_parents.push(parent.is_some());
            g.missing_parent.push(parent.is_some() && group.is_none());
            g.own_group.push(m.and_then(|m| {
                m.id.as_deref()
                    .and_then(|id| groups.get(&(m.namespace, id)).copied())
            }));
            if let Some(group) = group {
                g.children[group].push(i);
            }
        }
        g
    }
    /// Direct Parent permission from grounded, actually executed instructions.
    /// Roots remain permitted even when not currently emitted, so a previously
    /// suppressed root can reappear when a dependent suppressor is removed.
    pub fn permitted_by_executed(&self, executed: &[bool]) -> Result<Vec<bool>, &'static str> {
        if executed.len() != self.len() {
            return Err("dependency execution mask length mismatch");
        }
        let mut groups = vec![false; self.children.len()];
        for (i, &active) in executed.iter().enumerate() {
            if active {
                if let Some(group) = self.own_group[i] {
                    groups[group] = true;
                }
            }
        }
        let mut permitted: Vec<_> = self.declared_parents.iter().map(|v| !*v).collect();
        for (group, children) in self.children.iter().enumerate() {
            if groups[group] {
                for &child in children {
                    permitted[child] = true;
                }
            }
        }
        Ok(permitted)
    }

    /// Camera-independent fact used by backends to skip dependency trial passes.
    pub fn has_parents(&self) -> bool {
        self.has_parents
    }

    /// Retained allocation estimate, excluding allocator bookkeeping.
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.own_group.capacity() * std::mem::size_of::<Option<usize>>()
            + self.children.capacity() * std::mem::size_of::<Vec<usize>>()
            + self
                .children
                .iter()
                .map(|v| v.capacity() * std::mem::size_of::<usize>())
                .sum::<usize>()
            + self.missing_parent.capacity()
            + self.declared_parents.capacity()
    }

    pub fn len(&self) -> usize {
        self.own_group.len()
    }
    pub fn is_empty(&self) -> bool {
        self.own_group.is_empty()
    }
    /// `eligible` must be supplied from the backend's actual execution results
    /// after scale, viewing groups, dates, display-plane, line suppression,
    /// symbol resource failures and other rendering gates. A mere metadata
    /// visibility mask is insufficient. This method does not perform drawing.
    ///
    /// The least grounded solution cannot bootstrap an isolated dependency
    /// cycle. No recursion: O(N+G+string hashing) compile, O(N+G) resolution and
    /// storage, including long chains and repeated identifiers.
    pub fn resolve(&self, eligible: &[bool]) -> Result<DependencyResolution, &'static str> {
        if eligible.len() != self.len() {
            return Err("dependency execution mask length mismatch");
        }
        let mut executed = vec![false; self.len()];
        let mut groups = vec![false; self.children.len()];
        let mut queue = VecDeque::new();
        for i in 0..self.len() {
            if eligible[i] && !self.declared_parents[i] {
                executed[i] = true;
                queue.push_back(i);
            }
        }
        while let Some(i) = queue.pop_front() {
            if let Some(group) = self.own_group[i] {
                if groups[group] {
                    continue;
                }
                groups[group] = true;
                for &child in &self.children[group] {
                    if eligible[child] && !executed[child] {
                        executed[child] = true;
                        queue.push_back(child);
                    }
                }
            }
        }
        Ok(DependencyResolution {
            missing_parent_count: self.missing_parent.iter().filter(|v| **v).count(),
            ungrounded_count: eligible
                .iter()
                .zip(&executed)
                .filter(|(eligible, executed)| **eligible && !**executed)
                .count(),
            executed,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn m(ns: u64, id: Option<&str>, parent: Option<&str>) -> Option<DrawingDependency> {
        DrawingDependency::new(ns, id, parent, false)
    }
    #[test]
    fn repeated_ids_cross_features_forward_references_and_namespaces() {
        // Child precedes parents and belongs to a different feature in the
        // same display-list namespace. A hidden parent does not block its OR peer.
        let meta = [
            m(1, None, Some("a")),
            m(1, Some("a"), None),
            m(1, Some("a"), None),
            m(2, None, Some("a")),
            m(1, None, Some("absent")),
        ];
        let g = DrawingDependencyGraph::compile(meta.iter().map(Option::as_ref));
        let r = g.resolve(&[true, false, true, true, true]).unwrap();
        assert_eq!(r.executed, [true, false, true, false, false]);
        assert_eq!(r.missing_parent_count, 2);
        assert_eq!(
            g.resolve(&[true, false, false, true, true])
                .unwrap()
                .executed,
            [false, false, false, false, false]
        );
    }
    #[test]
    fn cycles_need_an_executed_root_and_all_execution_gates_are_respected() {
        let meta = [
            m(0, Some("a"), Some("b")),
            m(0, Some("b"), Some("a")),
            m(0, Some("b"), None),
            m(0, None, Some("a")),
        ];
        let g = DrawingDependencyGraph::compile(meta.iter().map(Option::as_ref));
        assert_eq!(
            g.resolve(&[true, true, false, true]).unwrap().executed,
            [false, false, false, false]
        );
        assert_eq!(
            g.resolve(&[true, true, true, true]).unwrap().executed,
            [true, true, true, true]
        );
        assert_eq!(
            g.resolve(&[false, true, true, true]).unwrap().executed,
            [false, false, true, false]
        );
        assert!(g.resolve(&[]).is_err());
    }
    #[test]
    fn hundred_thousand_chain_and_duplicate_group_are_linear_not_recursive() {
        let meta: Vec<_> = (0..100_000)
            .map(|i| {
                m(
                    0,
                    Some(&i.to_string()),
                    (i > 0).then(|| (i - 1).to_string()).as_deref(),
                )
            })
            .collect();
        let g = DrawingDependencyGraph::compile(meta.iter().map(Option::as_ref));
        let eligible = vec![true; meta.len()];
        assert!(g.resolve(&eligible).unwrap().executed.iter().all(|v| *v));
        let mut eligible = eligible;
        eligible[50_000] = false;
        let r = g.resolve(&eligible).unwrap();
        assert!(r.executed[..50_000].iter().all(|v| *v));
        assert!(r.executed[50_000..].iter().all(|v| !*v));
        let meta: Vec<_> = (0..100_000)
            .map(|i| m(0, Some("shared"), (i >= 50_000).then_some("shared")))
            .collect();
        let g = DrawingDependencyGraph::compile(meta.iter().map(Option::as_ref));
        assert_eq!(g.children.iter().map(Vec::len).sum::<usize>(), 50_000);
        assert!(g
            .resolve(&vec![true; 100_000])
            .unwrap()
            .executed
            .iter()
            .all(|v| *v));
    }
}

#[cfg(test)]
mod execution_permission_tests {
    use super::*;
    #[test]
    fn an_unemitted_root_remains_permitted_and_actual_parents_control_children() {
        let metadata = [
            DrawingDependency::new(1, Some("root"), None, false),
            DrawingDependency::new(1, Some("child"), Some("root"), false),
            DrawingDependency::new(1, Some("grandchild"), Some("child"), false),
        ];
        let g = DrawingDependencyGraph::compile(metadata.iter().map(Option::as_ref));
        assert_eq!(
            g.permitted_by_executed(&[false, false, false]).unwrap(),
            [true, false, false]
        );
        assert_eq!(
            g.permitted_by_executed(&[true, false, false]).unwrap(),
            [true, true, false]
        );
        assert_eq!(
            g.permitted_by_executed(&[true, true, false]).unwrap(),
            [true, true, true]
        );
        assert!(g.permitted_by_executed(&[]).is_err());
    }
}
