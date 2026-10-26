//! Invocation-local conservative preparation filter, never visibility authority.
//! Disabled/over-budget paths keep ALL ordinals for original authoritative checks.
pub(crate) enum Candidates {
    All,
    Selected(Vec<bool>),
}
/// Actual same-binary native trial showed no benefit from All. Keep the dense
/// original allocation by default; exact "0" opts into the experimental All path.
/// Called only at renderer creation. No environment lookup in frame/ordinal loops.
pub(crate) fn dense_baseline_flag(value: Option<&std::ffi::OsStr>) -> bool {
    value != Some(std::ffi::OsStr::new("0"))
}
impl Candidates {
    const CAP: usize = 1024 * 1024;
    pub(crate) fn prepare(
        enabled: bool,
        count: usize,
        dense_legacy: bool,
        configure: impl FnOnce(&mut [bool]),
    ) -> Self {
        if (!enabled && !dense_legacy) || count > Self::CAP / std::mem::size_of::<bool>() {
            return Self::All;
        }
        let mut mask = Vec::new();
        if mask.try_reserve_exact(count).is_err()
            || mask.capacity() > Self::CAP / std::mem::size_of::<bool>()
        {
            return Self::All;
        }
        mask.resize(count, true);
        if enabled {
            configure(&mut mask);
        }
        Self::Selected(mask)
    }
    #[inline]
    pub(crate) fn keeps(&self, ordinal: usize) -> bool {
        match self {
            Self::All => true,
            Self::Selected(mask) => mask[ordinal],
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dense_baseline_restores_only_original_all_true_allocation() {
        let c = Candidates::prepare(false, 7, true, |_| panic!("disabled hierarchy queried"));
        assert!(matches!(c, Candidates::Selected(_)));
        assert!((0..7).all(|i| c.keeps(i)));
        let c = Candidates::prepare(false, Candidates::CAP + 1, true, |_| unreachable!());
        assert!(matches!(c, Candidates::All));
    }
    #[test]
    fn experimental_all_requires_exact_explicit_zero() {
        assert!(dense_baseline_flag(None));
        for value in ["", "1", "true", "yes", "01", "00", " 0", "0 "] {
            let dense = dense_baseline_flag(Some(std::ffi::OsStr::new(value)));
            assert!(dense);
            // Default/invalid controls restore dense allocation without querying
            // hierarchy or changing original ordinal eligibility.
            let mask = Candidates::prepare(false, 7, dense, |_| panic!("disabled query"));
            assert!(matches!(mask, Candidates::Selected(_)));
            assert!((0..7).all(|i| mask.keeps(i)));
        }
        let dense = dense_baseline_flag(None);
        let mask = Candidates::prepare(false, 7, dense, |_| panic!("disabled query"));
        assert!(matches!(mask, Candidates::Selected(_)));
        assert!((0..7).all(|i| mask.keeps(i)));
        assert!(!dense_baseline_flag(Some(std::ffi::OsStr::new("0"))));
        let mask = Candidates::prepare(
            false,
            7,
            dense_baseline_flag(Some(std::ffi::OsStr::new("0"))),
            |_| panic!("experimental All queried hierarchy"),
        );
        assert!(matches!(mask, Candidates::All));
        assert!((0..7).all(|i| mask.keeps(i)));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(dense_baseline_flag(Some(std::ffi::OsStr::from_bytes(&[
                0xff
            ]))));
        }
    }
    #[test]
    fn disabled_path_never_allocates_or_prepares_spatial_state() {
        let c = Candidates::prepare(false, usize::MAX, false, |_| panic!("disabled callback"));
        assert!(matches!(c, Candidates::All));
        assert!(c.keeps(usize::MAX));
    }
    #[test]
    fn cap_decline_is_conservative_before_callback() {
        let c = Candidates::prepare(true, Candidates::CAP + 1, false, |_| {
            panic!("over-budget callback")
        });
        assert!(matches!(c, Candidates::All));
        assert!(c.keeps(0));
    }
    #[test]
    fn exact_ordinal_mask_matches_original_dense_oracle() {
        for n in 0..=10 {
            for subset in 0..(1usize << n) {
                // Frozen prior algorithm: all ordinary source ordinals kept,
                // indexed areas first cleared then query results enabled.
                let indexed = [1usize, 3, 6, 8];
                let mut oracle = vec![true; n];
                for id in indexed.into_iter().filter(|id| *id < n) {
                    oracle[id] = false;
                }
                for id in indexed.into_iter().filter(|id| *id < n) {
                    if subset & (1 << id) != 0 {
                        oracle[id] = true;
                    }
                }
                let actual = Candidates::prepare(true, n, false, |mask| {
                    for id in indexed.into_iter().filter(|id| *id < n) {
                        mask[id] = subset & (1 << id) != 0;
                    }
                });
                let expected: Vec<usize> = oracle
                    .iter()
                    .enumerate()
                    .filter_map(|(i, keep)| keep.then_some(i))
                    .collect();
                let observed: Vec<usize> = (0..n).filter(|i| actual.keeps(*i)).collect();
                assert_eq!(observed, expected);
                if let Candidates::Selected(mask) = actual {
                    assert!(mask.capacity() <= Candidates::CAP);
                } else {
                    panic!("small supported input unexpectedly declined");
                }
            }
        }
    }
    #[test]
    fn current_camera_result_cannot_reuse_old_rejections() {
        let old = Candidates::prepare(true, 3, false, |m| m[1] = false);
        let next = Candidates::prepare(true, 3, false, |m| m[2] = false);
        assert_eq!((old.keeps(1), old.keeps(2)), (false, true));
        assert_eq!((next.keeps(1), next.keeps(2)), (true, false));
    }
    #[test]
    fn failed_private_preparation_does_not_mutate_previous_filter() {
        let old = Candidates::prepare(true, 3, false, |m| m[1] = false);
        let failure = std::panic::catch_unwind(|| {
            Candidates::prepare(true, 3, false, |m| {
                m[1] = true;
                panic!("private preparation failed");
            })
        });
        assert!(failure.is_err());
        assert!(!old.keeps(1));
        assert!(old.keeps(0));
        assert!(old.keeps(2));
    }
    #[test]
    fn all_keeps_original_first_error_and_dispatch_order() {
        let c = Candidates::prepare(false, 5, false, |_| unreachable!());
        let mut visited = Vec::new();
        let result: Result<(), usize> = (0..5).filter(|i| c.keeps(*i)).try_for_each(|i| {
            visited.push(i);
            if i == 2 {
                Err(i)
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(2));
        assert_eq!(visited, [0, 1, 2]);
    }
}
