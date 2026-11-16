use super::{LockResult, MutexGuard};
use crate::rt;

use std::time::Duration;

/// Mock implementation of `std::sync::Condvar`.
#[derive(Debug)]
pub struct Condvar {
    object: rt::Condvar,
}

/// A type indicating whether a timed wait on a condition variable returned due
/// to a time out or not.
#[derive(Debug)]
pub struct WaitTimeoutResult(bool);

impl Condvar {
    /// Creates a new condition variable which is ready to be waited on and notified.
    pub fn new() -> Condvar {
        Condvar {
            object: rt::Condvar::new(),
        }
    }

    /// Blocks the current thread until this condition variable receives a notification.
    ///
    /// As with `std`, the wait may also end without one (a spurious wakeup),
    /// unless the model turns that off with
    /// [`Builder::spurious_wakeups`](crate::model::Builder::spurious_wakeups).
    #[track_caller]
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
        Ok(self.wait_inner(guard, false, location!()).0)
    }

    /// Waits, and returns whether the wait ended without a notification.
    fn wait_inner<'a, T>(
        &self,
        mut guard: MutexGuard<'a, T>,
        times_out: bool,
        location: rt::Location,
    ) -> (MutexGuard<'a, T>, bool) {
        // Release the RefCell borrow guard allowing another thread to lock the
        // data
        guard.unborrow();

        // Wait until notified, or until the wait ends without a notification
        let early = self.object.wait(guard.rt(), times_out, location);

        // Borrow the mutex guarded data again
        guard.reborrow();

        (guard, early)
    }

    /// Blocks the current thread until this condition variable receives a
    /// notification and the given condition returns false.
    #[track_caller]
    pub fn wait_while<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        mut condition: F,
    ) -> LockResult<MutexGuard<'a, T>>
    where
        F: FnMut(&mut T) -> bool,
    {
        while condition(&mut *guard) {
            guard = self.wait(guard)?;
        }
        Ok(guard)
    }

    /// Waits on this condition variable for a notification, timing out after a
    /// specified duration.
    ///
    /// Loom has no notion of time: the wait may time out whatever the
    /// duration, at any point at which it has not been notified yet. A
    /// thread times out at most once per condition variable in an
    /// execution, so that a loop that waits again after a timeout ends; a
    /// later timed wait of the thread on the same condition variable waits
    /// for a notification.
    #[track_caller]
    pub fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        _dur: Duration,
    ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)> {
        let (guard, timed_out) = self.wait_inner(guard, true, location!());

        Ok((guard, WaitTimeoutResult(timed_out)))
    }

    /// Waits on this condition variable for a notification and for the given
    /// condition to return false, timing out after a specified duration.
    ///
    /// The result says whether the wait timed out with the condition still
    /// true. See [`wait_timeout`](Condvar::wait_timeout) for when a wait times
    /// out.
    #[track_caller]
    pub fn wait_timeout_while<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        _dur: Duration,
        mut condition: F,
    ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)>
    where
        F: FnMut(&mut T) -> bool,
    {
        let location = location!();

        while condition(&mut *guard) {
            let (next, timed_out) = self.wait_inner(guard, true, location);
            guard = next;

            if timed_out {
                let still = condition(&mut *guard);
                return Ok((guard, WaitTimeoutResult(still)));
            }
        }

        Ok((guard, WaitTimeoutResult(false)))
    }

    /// Wakes up one blocked thread on this condvar.
    #[track_caller]
    pub fn notify_one(&self) {
        self.object.notify_one(location!());
    }

    /// Wakes up all blocked threads on this condvar.
    #[track_caller]
    pub fn notify_all(&self) {
        self.object.notify_all(location!());
    }
}

impl WaitTimeoutResult {
    /// Returns `true` if the wait was known to have timed out.
    pub fn timed_out(&self) -> bool {
        self.0
    }
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}
