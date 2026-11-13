use crate::rt::object;
use crate::rt::{thread, Access, Location, Synchronize, VersionVec};

use std::sync::atomic::Ordering::{Acquire, Release};

use tracing::trace;
#[derive(Debug, Copy, Clone)]
pub(crate) struct Mutex {
    state: object::Ref<State>,
}

#[derive(Debug)]
pub(super) struct State {
    /// If the mutex should establish sequential consistency.
    seq_cst: bool,

    /// `Some` when the mutex is in the locked state. The `thread::Id`
    /// references the thread that currently holds the mutex.
    lock: Option<thread::Id>,

    /// Tracks access to the mutex
    last_access: Option<Access>,

    /// Tracks the last time the mutex was released
    last_release_access: Option<Access>,

    /// Causality transfers between threads
    synchronize: Synchronize,
}

impl Mutex {
    pub(crate) fn new(seq_cst: bool) -> Mutex {
        super::execution(|execution| {
            let state = execution.objects.insert(State {
                seq_cst,
                lock: None,
                last_access: None,
                last_release_access: None,
                synchronize: Synchronize::new(),
            });

            trace!(?state, ?seq_cst, "Mutex::new");

            Mutex { state }
        })
    }

    pub(crate) fn acquire_lock(&self, location: Location) {
        self.state.branch_acquire(self.is_locked(), location);
        assert!(self.post_acquire(), "expected to be able to acquire lock");
    }

    pub(crate) fn try_acquire_lock(&self, location: Location) -> bool {
        self.state.branch_opaque_non_blocking(location);
        self.post_acquire()
    }

    /// Release the lock, as a point at which other threads may be scheduled
    /// while the lock is still held.
    ///
    /// Acquiring the lock branches *before* it is taken, so without a branch
    /// here nothing could ever run between a thread's acquire and its
    /// release, unless the holder happened to do something else in between.
    pub(crate) fn unlock(&self, location: Location) {
        let can_branch = !std::thread::panicking()
            && super::execution(|execution| execution.threads.is_active());

        if can_branch {
            self.state.branch_release(location);
        }

        self.release_lock();
    }

    pub(crate) fn release_lock(&self) {
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            // Release the lock flag
            state.lock = None;

            // Execution has deadlocked, cleanup does not matter.
            if !execution.threads.is_active() {
                return;
            }

            state
                .synchronize
                .sync_store(&mut execution.threads, Release);

            if state.seq_cst {
                // Establish sequential consistency between the lock's operations.
                execution.threads.seq_cst();
            }

            let thread_id = execution.threads.active_id();

            for (id, thread) in execution.threads.iter_mut() {
                if id == thread_id {
                    continue;
                }

                let obj = thread
                    .operation
                    .as_ref()
                    .map(|operation| operation.object());

                if obj == Some(self.state.erase()) {
                    trace!(state = ?self.state, thread = ?id,
                        "Mutex::release_lock");
                    thread.set_runnable();
                }
            }
        });
    }

    fn post_acquire(&self) -> bool {
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            let thread_id = execution.threads.active_id();

            if state.lock.is_some() {
                return false;
            }

            // Set the lock to the current thread
            state.lock = Some(thread_id);

            dbg!(state.synchronize.sync_load(&mut execution.threads, Acquire));

            if state.seq_cst {
                // Establish sequential consistency between locks
                execution.threads.seq_cst();
            }

            // Block all **other** threads attempting to acquire the mutex
            for (id, thread) in execution.threads.iter_mut() {
                if id == thread_id {
                    continue;
                }

                if let Some(operation) = thread.operation.as_ref() {
                    // A thread about to `try_lock` does not wait for the
                    // mutex; it will simply observe that it is locked.
                    if operation.object() == self.state.erase()
                        && operation.action() == object::Action::Opaque
                    {
                        let location = operation.location();
                        trace!(state = ?self.state, thread = ?id, %location,
                            "Mutex::post_acquire");
                        thread.set_blocked(location);
                    }
                }
            }

            true
        })
    }

    /// Returns `true` if the mutex is currently locked
    pub(crate) fn is_locked(&self) -> bool {
        super::execution(|execution| {
            let is_locked = self.state.get(&execution.objects).lock.is_some();

            trace!(state = ?self.state, ?is_locked, "Mutex::is_locked");

            is_locked
        })
    }
}

impl State {
    /// Returns the previous accesses that `action` is dependent with.
    ///
    /// Releasing the lock is tracked separately from acquiring it. A thread
    /// that *waits* for the lock cannot run between another thread's acquire
    /// and release, so with `racing` set a blocking acquire does not report
    /// the last release: there is nothing to reorder. An attempt (`try_*`)
    /// can run there, and observes the lock as held if it does.
    pub(super) fn dependent_accesses(
        &self,
        action: object::Action,
        racing: bool,
    ) -> impl Iterator<Item = &Access> {
        let release = if racing && !Self::is_attempt(action) {
            None
        } else {
            self.last_release_access.as_ref()
        };

        self.last_access.iter().chain(release)
    }

    pub(crate) fn set_last_access(
        &mut self,
        action: object::Action,
        path_id: usize,
        version: &VersionVec,
    ) {
        let access = if Self::is_release(action) {
            &mut self.last_release_access
        } else {
            &mut self.last_access
        };

        Access::set_or_create(access, path_id, version);
    }

    fn is_attempt(action: object::Action) -> bool {
        action == object::Action::OpaqueNonBlocking
    }

    fn is_release(action: object::Action) -> bool {
        action == object::Action::Release
    }
}
