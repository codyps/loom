use crate::rt::object;
use crate::rt::{self, thread, Access, Mutex, ThreadSet, VersionVec};

use std::collections::VecDeque;

use tracing::trace;

use super::Location;

#[derive(Debug, Copy, Clone)]
pub(crate) struct Condvar {
    state: object::Ref<State>,
}

#[derive(Debug)]
pub(super) struct State {
    /// Tracks access to the mutex
    last_access: Option<Access>,

    /// Threads waiting on the condvar
    waiters: VecDeque<thread::Id>,

    /// Threads that have stopped waiting without a notification once, by
    /// timing out or spuriously. Each does so at most once, so that a loop
    /// around a wait ends.
    woke_early: ThreadSet,
}

impl Condvar {
    /// Create a new condition variable object
    pub(crate) fn new() -> Condvar {
        super::execution(|execution| {
            let state = execution.objects.insert(State {
                last_access: None,
                waiters: VecDeque::new(),
                woke_early: 0,
            });

            trace!(?state, "Condvar::new");

            Condvar { state }
        })
    }

    /// Blocks the current thread until this condition variable receives a
    /// notification, or the wait ends without one: when it `times_out`, or
    /// spuriously, if the model allows that. Returns `true` if it ended
    /// without a notification.
    pub(crate) fn wait(&self, mutex: &Mutex, times_out: bool, location: Location) -> bool {
        self.state.branch_opaque(location);

        let early = rt::execution(|execution| {
            trace!(state = ?self.state, ?mutex, %location, "Condvar::wait");

            let thread = execution.threads.active_id();
            let bit: ThreadSet = 1 << thread.as_usize();
            let may_end_early = (times_out || execution.spurious_wakeups)
                && self.state.get(&execution.objects).woke_early & bit == 0;

            let early = may_end_early && execution.path.branch_spurious();

            let state = self.state.get_mut(&mut execution.objects);

            if early {
                state.woke_early |= bit;
            }

            // Track the current thread as a waiter
            state.waiters.push_back(thread);

            early
        });

        // Release the lock
        mutex.release_lock();

        if early {
            // The thread does not block. Time passes, or nothing does, and
            // other threads may run before the wait ends, and notify it: this
            // is a point at which they can, dependent with notifications.
            self.state.branch_opaque(location);

            let ended_early = rt::execution(|execution| {
                let thread = execution.threads.active_id();
                let state = self.state.get_mut(&mut execution.objects);

                // Still waiting: it ends here, without a notification.
                match state.waiters.iter().position(|&waiter| waiter == thread) {
                    Some(pos) => {
                        state.waiters.remove(pos);
                        true
                    }
                    None => false,
                }
            });

            trace!(state = ?self.state, ?ended_early, %location, "Condvar::wait ends early");

            mutex.acquire_lock(location);

            return ended_early;
        }

        // Wait to be taken off the waiter list by a notification. This is the
        // condvar's own wait: it does not go through `thread::park`, so it
        // neither consumes an unpark token nor is it ended by `unpark`.
        //
        // Nothing can run between queueing above and blocking here, so the
        // thread is still queued, but there is no reason to rely on that.
        let queued = rt::execution(|execution| {
            let thread = execution.threads.active_id();
            let state = self.state.get(&execution.objects);

            state.waiters.contains(&thread)
        });

        if queued {
            rt::block(location);
        }

        // Acquire the lock again
        mutex.acquire_lock(location);

        false
    }

    /// Wakes up one blocked thread on this condvar.
    pub(crate) fn notify_one(&self, location: Location) {
        self.state.branch_opaque(location);

        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            // Notify the first waiter
            let thread = state.waiters.pop_front();

            trace!(state = ?self.state, ?thread, %location, "Condvar::notify_one");

            if let Some(thread) = thread {
                execution.threads.wake(thread);
            }
        })
    }

    /// Wakes up all blocked threads on this condvar.
    pub(crate) fn notify_all(&self, location: Location) {
        self.state.branch_opaque(location);

        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            trace!(state = ?self.state, threads = ?state.waiters, %location, "Condvar::notify_all");

            for thread in state.waiters.drain(..) {
                execution.threads.wake(thread);
            }
        })
    }
}

impl State {
    pub(super) fn last_dependent_access(&self) -> Option<&Access> {
        self.last_access.as_ref()
    }

    pub(crate) fn set_last_access(&mut self, path_id: usize, version: &VersionVec) {
        Access::set_or_create(&mut self.last_access, path_id, version);
    }
}
