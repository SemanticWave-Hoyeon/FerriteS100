//! Diagnostic collectors shared between an emission and its owner. Same call
//! shape as `Rc<RefCell<T>>`/`Rc<Cell<T>>`, but `Send`, so an emission owning
//! one may be prepared on a worker thread. A nested borrow blocks instead of
//! panicking; no caller holds one borrow across another (RefCell forbade it).
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) struct Shared<T>(Arc<Mutex<T>>);

impl<T> Shared<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(value)))
    }
    /// Counters stay readable after a panicking writer; they are never authority.
    pub(crate) fn borrow(&self) -> MutexGuard<'_, T> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    pub(crate) fn borrow_mut(&self) -> MutexGuard<'_, T> {
        self.borrow()
    }
}
impl<T: Copy> Shared<T> {
    pub(crate) fn get(&self) -> T {
        *self.borrow()
    }
    pub(crate) fn set(&self, value: T) {
        *self.borrow() = value;
    }
}
impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.borrow().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clones_share_one_value_across_threads() {
        let a = Shared::new(1u64);
        let b = a.clone();
        std::thread::spawn(move || b.set(b.get() + 1))
            .join()
            .unwrap();
        assert_eq!(a.get(), 2);
        a.borrow_mut().clone_from(&5);
        assert_eq!(*a.borrow(), 5);
    }
}
