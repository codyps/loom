//! Mock implementation of the `lock_api` raw lock traits, and through them of
//! the `lock_api` / `parking_lot` style `Mutex` and `RwLock`.
//!
//! These locks differ from the `std` ones mocked in [`crate::sync`]: no
//! poisoning, guards that can be mapped, and locks that can be created in
//! `const` contexts. Code written against `parking_lot` or `lock_api` can use
//! the type aliases in this module under `cfg(loom)`.
//!
//! _These types are only available if loom is built with the `"lock_api"`
//! feature._
//!
//! Like every loom object, a lock is tied to the execution it is first used
//! in. In particular a lock kept in a `static` cannot be shared between
//! executions of a model.

use crate::rt;

use std::sync::OnceLock;

/// Mock implementation of a `lock_api::RawMutex`.
pub struct RawMutex {
    // `lock_api::RawMutex` requires a `const INIT`, but loom objects can only
    // be created while a model is running, so the modeled mutex is created on
    // first use.
    object: OnceLock<rt::Mutex>,
}

impl RawMutex {
    fn object(&self) -> &rt::Mutex {
        self.object.get_or_init(|| rt::Mutex::new(true))
    }
}

impl std::fmt::Debug for RawMutex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawMutex").finish_non_exhaustive()
    }
}

unsafe impl lock_api::RawMutex for RawMutex {
    // The lint fires for any `const` with interior mutability; a fresh,
    // unlocked lock per use is exactly what `INIT` is for.
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: RawMutex = RawMutex {
        object: OnceLock::new(),
    };

    type GuardMarker = lock_api::GuardNoSend;

    #[track_caller]
    fn lock(&self) {
        self.object().acquire_lock(location!());
    }

    #[track_caller]
    fn try_lock(&self) -> bool {
        self.object().try_acquire_lock(location!())
    }

    unsafe fn unlock(&self) {
        self.object().release_lock()
    }

    fn is_locked(&self) -> bool {
        self.object().is_locked()
    }
}

/// Mock implementation of a `lock_api::RawRwLock`.
pub struct RawRwLock {
    // See `RawMutex::object`.
    object: OnceLock<rt::RwLock>,
}

impl RawRwLock {
    fn object(&self) -> &rt::RwLock {
        self.object.get_or_init(rt::RwLock::new)
    }
}

impl std::fmt::Debug for RawRwLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawRwLock").finish_non_exhaustive()
    }
}

unsafe impl lock_api::RawRwLock for RawRwLock {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: RawRwLock = RawRwLock {
        object: OnceLock::new(),
    };

    type GuardMarker = lock_api::GuardNoSend;

    #[track_caller]
    fn lock_shared(&self) {
        self.object().acquire_read_lock(location!())
    }

    #[track_caller]
    fn try_lock_shared(&self) -> bool {
        self.object().try_acquire_read_lock(location!())
    }

    unsafe fn unlock_shared(&self) {
        self.object().release_read_lock()
    }

    #[track_caller]
    fn lock_exclusive(&self) {
        self.object().acquire_write_lock(location!())
    }

    #[track_caller]
    fn try_lock_exclusive(&self) -> bool {
        self.object().try_acquire_write_lock(location!())
    }

    unsafe fn unlock_exclusive(&self) {
        self.object().release_write_lock()
    }

    fn is_locked(&self) -> bool {
        let object = self.object();
        object.is_read_locked() || object.is_write_locked()
    }

    fn is_locked_exclusive(&self) -> bool {
        self.object().is_write_locked()
    }
}

/// Mock implementation of `lock_api::Mutex` (and `parking_lot::Mutex`).
pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;

/// Mock implementation of `lock_api::MutexGuard`.
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutex, T>;

/// Mock implementation of `lock_api::MappedMutexGuard`.
pub type MappedMutexGuard<'a, T> = lock_api::MappedMutexGuard<'a, RawMutex, T>;

/// Mock implementation of `lock_api::RwLock` (and `parking_lot::RwLock`).
pub type RwLock<T> = lock_api::RwLock<RawRwLock, T>;

/// Mock implementation of `lock_api::RwLockReadGuard`.
pub type RwLockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawRwLock, T>;

/// Mock implementation of `lock_api::RwLockWriteGuard`.
pub type RwLockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawRwLock, T>;

/// Mock implementation of `lock_api::MappedRwLockReadGuard`.
pub type MappedRwLockReadGuard<'a, T> = lock_api::MappedRwLockReadGuard<'a, RawRwLock, T>;

/// Mock implementation of `lock_api::MappedRwLockWriteGuard`.
pub type MappedRwLockWriteGuard<'a, T> = lock_api::MappedRwLockWriteGuard<'a, RawRwLock, T>;
