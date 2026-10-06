//! S-100 ordered-list updates. Indices address items (coordinate tuples,
//! segments, or component references), never bytes or coordinate ordinates.
use anyhow::{ensure, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateInstruction {
    Insert,
    Delete,
    Modify,
}
impl UpdateInstruction {
    pub fn parse(value: u8) -> Result<Self> {
        Ok(match value {
            1 => Self::Insert,
            2 => Self::Delete,
            3 => Self::Modify,
            _ => anyhow::bail!("Unknown S-100 update instruction {value}"),
        })
    }
}

/// S-100 Part 10a COCC, SECC, and CCOC use the same list-control semantics.
#[derive(Debug, Clone, Copy)]
pub struct SequenceControl {
    pub instruction: UpdateInstruction,
    pub index: u16,
    pub count: u16,
}
impl SequenceControl {
    /// Validate before allocation or mutation. Delete has a nonzero addressed
    /// count and NO payload; insert and modify carry exactly `count` items.
    pub fn addressed_range(
        &self,
        target_len: usize,
        payload_len: usize,
        max_items: usize,
    ) -> Result<std::ops::Range<usize>> {
        ensure!(self.index > 0 && self.count > 0, "Zero update index/count");
        let start = usize::from(self.index) - 1;
        let count = usize::from(self.count);
        ensure!(target_len <= max_items, "Target exceeds item budget");
        match self.instruction {
            UpdateInstruction::Insert => {
                ensure!(payload_len == count, "Insert payload count mismatch");
                ensure!(start <= target_len, "Insert index outside target");
                ensure!(
                    target_len
                        .checked_add(count)
                        .is_some_and(|n| n <= max_items),
                    "Insert exceeds item budget"
                );
                Ok(start..start)
            }
            UpdateInstruction::Delete | UpdateInstruction::Modify => {
                ensure!(
                    payload_len
                        == if self.instruction == UpdateInstruction::Delete {
                            0
                        } else {
                            count
                        },
                    "Delete/modify payload count mismatch"
                );
                let end = start
                    .checked_add(count)
                    .ok_or_else(|| anyhow::anyhow!("Update range overflow"))?;
                ensure!(end <= target_len, "Update range outside target");
                Ok(start..end)
            }
        }
    }
    /// O(target + payload) time and bounded output memory. A failure leaves the
    /// input untouched, including an allocation refusal.
    pub fn applied<T: Clone>(
        &self,
        target: &[T],
        payload: &[T],
        max_items: usize,
    ) -> Result<Vec<T>> {
        let range = self.addressed_range(target.len(), payload.len(), max_items)?;
        let len = target.len() - range.len() + payload.len();
        let mut out = Vec::new();
        out.try_reserve_exact(len)?;
        out.extend_from_slice(&target[..range.start]);
        out.extend_from_slice(payload);
        out.extend_from_slice(&target[range.end..]);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latest_indices_empty_delete_and_tail_insert() {
        let a = vec![10, 20, 30];
        let b = SequenceControl {
            instruction: UpdateInstruction::Insert,
            index: 2,
            count: 2,
        }
        .applied(&a, &[11, 12], 6)
        .unwrap();
        assert_eq!(b, [10, 11, 12, 20, 30]);
        let c = SequenceControl {
            instruction: UpdateInstruction::Delete,
            index: 2,
            count: 3,
        }
        .applied(&b, &[], 6)
        .unwrap();
        assert_eq!(c, [10, 30]);
        let d = SequenceControl {
            instruction: UpdateInstruction::Modify,
            index: 2,
            count: 1,
        }
        .applied(&c, &[99], 6)
        .unwrap();
        let e = SequenceControl {
            instruction: UpdateInstruction::Insert,
            index: 3,
            count: 1,
        }
        .applied(&d, &[100], 6)
        .unwrap();
        assert_eq!(e, [10, 99, 100]);
        assert_eq!(a, [10, 20, 30]);
    }
    #[test]
    fn invalid_controls_and_budget_never_change_input() {
        let a = vec![10, 20, 30];
        for (op, index, count, payload, max) in [
            (UpdateInstruction::Delete, 0, 1, vec![], 5),
            (UpdateInstruction::Delete, 1, 0, vec![], 5),
            (UpdateInstruction::Delete, 1, 1, vec![99], 5),
            (UpdateInstruction::Delete, 3, 2, vec![], 5),
            (UpdateInstruction::Modify, 1, 2, vec![99], 5),
            (UpdateInstruction::Insert, 5, 1, vec![99], 5),
            (UpdateInstruction::Insert, 4, 1, vec![99], 3),
        ] {
            assert!(SequenceControl {
                instruction: op,
                index,
                count
            }
            .applied(&a, &payload, max)
            .is_err());
        }
        assert_eq!(a, [10, 20, 30]);
    }
}
