use super::Atomic;

use std::sync::atomic::Ordering;

/// Mock implementation of `std::sync::atomic::AtomicPtr`.
///
/// NOTE: Unlike `std::sync::atomic::AtomicPtr`, this type has a different
/// in-memory representation than `*mut T`.
pub struct AtomicPtr<T>(Atomic<*mut T>);

impl<T> std::fmt::Debug for AtomicPtr<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> AtomicPtr<T> {
    /// Creates a new instance of `AtomicPtr`.
    #[track_caller]
    pub fn new(v: *mut T) -> AtomicPtr<T> {
        AtomicPtr(Atomic::new(v, location!()))
    }

    /// Load the value without any synchronization.
    ///
    /// # Safety
    ///
    /// An unsynchronized atomic load technically always has undefined behavior.
    /// However, if the atomic value is not currently visible by other threads,
    /// this *should* always be equivalent to a non-atomic load of an un-shared
    /// `*mut T` value.
    pub unsafe fn unsync_load(&self) -> *mut T {
        self.0.unsync_load()
    }

    /// Get access to a mutable reference to the inner value.
    #[track_caller]
    pub fn with_mut<R>(&mut self, f: impl FnOnce(&mut *mut T) -> R) -> R {
        self.0.with_mut(f)
    }

    /// Consumes the atomic and returns the contained value.
    #[track_caller]
    pub fn into_inner(self) -> *mut T {
        // SAFETY: ownership guarantees that no other threads are concurrently
        // accessing the atomic value.
        unsafe { self.unsync_load() }
    }

    /// Loads a value from the pointer.
    #[track_caller]
    pub fn load(&self, order: Ordering) -> *mut T {
        self.0.load(order)
    }

    /// Loads a value from the pointer, with what C++ calls
    /// `memory_order_consume`: only what is done *with the pointer* comes
    /// after the store that is read. This is what reading an RCU-protected
    /// pointer needs, and all it needs.
    ///
    /// `std` has no such ordering, and no such method. Code gets it the way
    /// `crossbeam_utils::atomic::AtomicConsume` does: a `Relaxed` load where
    /// the processor keeps dependent accesses in order by itself (Arm), an
    /// `Acquire` load elsewhere. This method stands in for that under loom.
    ///
    /// # What loom checks
    ///
    /// The load itself is a `Relaxed` one: it may return any store that a
    /// `Relaxed` load may, and the thread does not come to know anything
    /// by it. So code that needs more than a consume load provides, and
    /// would get it from the `Acquire` load that a compiler makes of one,
    /// fails under loom.
    ///
    /// What depends on the load is a guess, as loom does not follow values.
    /// An access to a loom atomic or `UnsafeCell` is taken to carry a
    /// dependency from the load if the object
    ///
    /// - lies in the `size_of::<T>()` bytes the pointer points to, or
    /// - was created before the store that was read, and after everything
    ///   the thread knows of: the thread must have got to it by a pointer
    ///   that it did not synchronize for. This finds what is behind a second
    ///   pointer, such as a `Box` or an `Arc` in `T`.
    ///
    /// Such an access comes after the store, and what the thread does next
    /// does not: a read of an `UnsafeCell` or a load that is not `SeqCst`
    /// leaves the thread knowing what it knew before, and what the load
    /// acquired by itself. A store, a read-modify-write and a `SeqCst` load
    /// are the exception. They leave something behind that says what they
    /// came after, so the thread comes after the consumed store from then
    /// on, as after an `Acquire` load of it, which is more than the
    /// ordering provides.
    ///
    /// The guess is wrong both ways. An object that is not in `T`, that the
    /// thread already knew of, and that was written to before the store is
    /// not taken to depend on the load when it is reached through it, and
    /// loom may report a race or a stale value there that cannot happen. An
    /// object that the thread got to by a `Relaxed` load of some other
    /// pointer, which is a bug, is taken to depend on the consume load if
    /// it was created before the store. Nor does loom know when the memory
    /// is freed: an object that is later allocated at the same address is
    /// taken for a part of `T` until the thread consumes another pointer to
    /// that memory.
    ///
    /// A load that returns a value yet to be stored (see
    /// [`Builder::load_buffering`](crate::model::Builder::load_buffering))
    /// is not explored for a consume load.
    #[track_caller]
    pub fn load_consume(&self) -> *mut T {
        self.0.load_consume(std::mem::size_of::<T>())
    }

    /// Stores a value into the pointer.
    #[track_caller]
    pub fn store(&self, val: *mut T, order: Ordering) {
        self.0.store(val, order)
    }

    /// Stores a value into the pointer, returning the previous value.
    #[track_caller]
    pub fn swap(&self, val: *mut T, order: Ordering) -> *mut T {
        self.0.swap(val, order)
    }

    /// Stores a value into the pointer if the current value is the same as the `current` value.
    #[track_caller]
    pub fn compare_and_swap(&self, current: *mut T, new: *mut T, order: Ordering) -> *mut T {
        self.0.compare_and_swap(current, new, order)
    }

    /// Stores a value into the pointer if the current value is the same as the `current` value.
    #[track_caller]
    pub fn compare_exchange(
        &self,
        current: *mut T,
        new: *mut T,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*mut T, *mut T> {
        self.0.compare_exchange(current, new, success, failure)
    }

    /// Stores a value into the atomic if the current value is the same as
    /// the `current` value.
    ///
    /// Unlike `compare_exchange`, this is allowed to fail even when the
    /// comparison succeeds, and loom explores that: where the exchange would
    /// succeed, it also tries the execution in which it fails, returning the
    /// current value as the error. Each thread fails spuriously at most once
    /// per atomic in an execution, so a loop that retries until the exchange
    /// succeeds still terminates.
    #[track_caller]
    pub fn compare_exchange_weak(
        &self,
        current: *mut T,
        new: *mut T,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*mut T, *mut T> {
        self.0.compare_exchange_weak(current, new, success, failure)
    }

    /// Fetches the value, and applies a function to it that returns an optional new value. Returns
    /// a [`Result`] of [`Ok`]`(previous_value)` if the function returned [`Some`]`(_)`, else
    /// [`Err`]`(previous_value)`.
    #[track_caller]
    pub fn try_update(
        &self,
        set_order: Ordering,
        fetch_order: Ordering,
        f: impl FnMut(*mut T) -> Option<*mut T>,
    ) -> Result<*mut T, *mut T> {
        self.0.try_update(set_order, fetch_order, f)
    }

    // TODO: Mark `#[deprecated]` once MSRV >= 1.99, to match std.
    /// An alias for [`try_update`](Self::try_update).
    #[track_caller]
    pub fn fetch_update<F>(
        &self,
        set_order: Ordering,
        fetch_order: Ordering,
        f: F,
    ) -> Result<*mut T, *mut T>
    where
        F: FnMut(*mut T) -> Option<*mut T>,
    {
        self.try_update(set_order, fetch_order, f)
    }

    /// Fetches the value, and applies a function to it that returns a new value. The new value
    /// is stored and the old value is returned.
    #[track_caller]
    pub fn update(
        &self,
        set_order: Ordering,
        fetch_order: Ordering,
        f: impl FnMut(*mut T) -> *mut T,
    ) -> *mut T {
        self.0.update(set_order, fetch_order, f)
    }

    /// Offsets the pointer's address by adding `val` (in units of `T`),
    /// returning the previous pointer.
    #[track_caller]
    pub fn fetch_ptr_add(&self, val: usize, order: Ordering) -> *mut T {
        self.0.rmw(|ptr| ptr.wrapping_add(val), order)
    }

    /// Offsets the pointer's address by subtracting `val` (in units of `T`),
    /// returning the previous pointer.
    #[track_caller]
    pub fn fetch_ptr_sub(&self, val: usize, order: Ordering) -> *mut T {
        self.0.rmw(|ptr| ptr.wrapping_sub(val), order)
    }

    /// Offsets the pointer's address by adding `val` *bytes*, returning the
    /// previous pointer.
    #[track_caller]
    pub fn fetch_byte_add(&self, val: usize, order: Ordering) -> *mut T {
        self.0
            .rmw(|ptr| ptr.cast::<u8>().wrapping_add(val).cast(), order)
    }

    /// Offsets the pointer's address by subtracting `val` *bytes*, returning the
    /// previous pointer.
    #[track_caller]
    pub fn fetch_byte_sub(&self, val: usize, order: Ordering) -> *mut T {
        self.0
            .rmw(|ptr| ptr.cast::<u8>().wrapping_sub(val).cast(), order)
    }

    /// Performs a bitwise "and" operation on the address of the current
    /// pointer, and the argument `val`, and stores a pointer with provenance of
    /// the current pointer and the resulting address.
    #[track_caller]
    pub fn fetch_and(&self, val: usize, order: Ordering) -> *mut T {
        self.0.rmw(|ptr| map_addr(ptr, |a| a & val), order)
    }

    /// Performs a bitwise "or" operation on the address of the current pointer,
    /// and the argument `val`, and stores a pointer with provenance of the
    /// current pointer and the resulting address.
    #[track_caller]
    pub fn fetch_or(&self, val: usize, order: Ordering) -> *mut T {
        self.0.rmw(|ptr| map_addr(ptr, |a| a | val), order)
    }

    /// Performs a bitwise "xor" operation on the address of the current pointer,
    /// and the argument `val`, and stores a pointer with provenance of the
    /// current pointer and the resulting address.
    #[track_caller]
    pub fn fetch_xor(&self, val: usize, order: Ordering) -> *mut T {
        self.0.rmw(|ptr| map_addr(ptr, |a| a ^ val), order)
    }
}

/// Changes the address of `ptr` while keeping its provenance.
///
/// Equivalent to `<*mut T>::map_addr`, which is newer than loom's minimum
/// supported Rust version.
fn map_addr<T>(ptr: *mut T, f: impl FnOnce(usize) -> usize) -> *mut T {
    let addr = ptr as usize;
    let offset = f(addr).wrapping_sub(addr);

    ptr.cast::<u8>().wrapping_add(offset).cast()
}

impl<T> Default for AtomicPtr<T> {
    fn default() -> AtomicPtr<T> {
        use std::ptr;
        AtomicPtr::new(ptr::null_mut())
    }
}

impl<T> From<*mut T> for AtomicPtr<T> {
    fn from(p: *mut T) -> Self {
        Self::new(p)
    }
}
