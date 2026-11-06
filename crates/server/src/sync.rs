use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn read<T: ?Sized>(rwlock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    rwlock.read().unwrap_or_else(PoisonError::into_inner)
}

pub fn write<T: ?Sized>(rwlock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    rwlock.write().unwrap_or_else(PoisonError::into_inner)
}

#[must_use = "the closure runs when this guard is dropped; binding it to `_` drops it at once"]
pub struct Defer<F: FnOnce()>(Option<F>);

pub fn defer<F: FnOnce()>(cleanup: F) -> Defer<F> {
    Defer(Some(cleanup))
}

impl<F: FnOnce()> Drop for Defer<F> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.0.take() {
            cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_is_still_usable() {
        let mutex = Mutex::new(1);
        let _ = std::panic::catch_unwind(|| {
            let _guard = mutex.lock();
            panic!("poison it");
        });
        assert!(mutex.is_poisoned());
        *lock(&mutex) += 1;
        assert_eq!(*lock(&mutex), 2);
    }

    #[test]
    fn defer_runs_on_drop() {
        let ran = std::cell::Cell::new(false);
        drop(defer(|| ran.set(true)));
        assert!(ran.get());
    }
}
