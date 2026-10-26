//! Data-parallel iteration switch shared by the chart crates.
//!
//! With the `parallel` feature this is rayon. Without it the same call sites
//! (`use ferrite_kernel::par::prelude::*`) run sequentially on the calling
//! thread: `par_iter`/`par_chunks` become the slice iterators, the sorts the
//! slice sorts and `with_min_len` does nothing. Every use collects in input
//! order, so results are identical either way.
#[cfg(feature = "parallel")]
pub use rayon::{current_num_threads, prelude};

/// Threads a parallel operation may use (1 without the `parallel` feature).
#[cfg(not(feature = "parallel"))]
pub fn current_num_threads() -> usize {
    1
}

/// Whether data-parallel iteration runs on several threads.
pub const ENABLED: bool = cfg!(feature = "parallel");

#[cfg(not(feature = "parallel"))]
pub mod prelude {
    use std::cmp::Ordering;

    /// `par_iter` on slices (and through deref, vectors).
    pub trait IntoParallelRefIterator<'a> {
        type Iter: Iterator;
        fn par_iter(&'a self) -> Self::Iter;
    }
    impl<'a, T: 'a> IntoParallelRefIterator<'a> for [T] {
        type Iter = std::slice::Iter<'a, T>;
        fn par_iter(&'a self) -> Self::Iter {
            self.iter()
        }
    }

    /// `par_chunks` on slices.
    pub trait ParallelSlice<T> {
        fn par_chunks(&self, size: usize) -> std::slice::Chunks<'_, T>;
    }
    impl<T> ParallelSlice<T> for [T] {
        fn par_chunks(&self, size: usize) -> std::slice::Chunks<'_, T> {
            self.chunks(size)
        }
    }

    /// Sorting slices in place.
    pub trait ParallelSliceMut<T> {
        fn par_sort_unstable_by<F: FnMut(&T, &T) -> Ordering>(&mut self, compare: F);
        fn par_sort_by<F: FnMut(&T, &T) -> Ordering>(&mut self, compare: F);
    }
    impl<T> ParallelSliceMut<T> for [T] {
        fn par_sort_unstable_by<F: FnMut(&T, &T) -> Ordering>(&mut self, compare: F) {
            self.sort_unstable_by(compare)
        }
        fn par_sort_by<F: FnMut(&T, &T) -> Ordering>(&mut self, compare: F) {
            self.sort_by(compare)
        }
    }

    /// Splitting hints, meaningless sequentially.
    pub trait IndexedParallelIterator: Iterator + Sized {
        fn with_min_len(self, _min: usize) -> Self {
            self
        }
        fn with_max_len(self, _max: usize) -> Self {
            self
        }
    }
    impl<I: Iterator> IndexedParallelIterator for I {}
}

#[cfg(test)]
mod tests {
    use super::prelude::*;

    #[test]
    fn collects_in_input_order() {
        let items: Vec<u32> = (0..10_000).collect();
        let doubled: Vec<u32> = items.par_iter().with_min_len(16).map(|i| i * 2).collect();
        assert_eq!(doubled, items.iter().map(|i| i * 2).collect::<Vec<_>>());
        let sums: Vec<u32> = items.par_chunks(1000).map(|c| c.iter().sum()).collect();
        assert_eq!(sums.len(), 10);
        let mut sorted = vec![3, 1, 2];
        sorted.par_sort_unstable_by(|a, b| a.cmp(b));
        assert_eq!(sorted, [1, 2, 3]);
        assert!(super::current_num_threads() >= 1);
    }
}
