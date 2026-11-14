use crate::rt::object;
use crate::rt::{thread, Access, Execution, Location, Synchronize, VersionVec};

use std::collections::HashMap;
use std::sync::atomic::Ordering::{Acquire, Release};

#[derive(Debug, Copy, Clone)]
pub(crate) struct RwLock {
    state: object::Ref<State>,
}

#[derive(Debug, PartialEq)]
enum Locked {
    /// Number of read locks held by each reader thread.
    Read(HashMap<thread::Id, usize>),
    Write(thread::Id),
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub(super) enum Action {
    /// Read lock
    Read,

    /// Write lock
    Write,

    /// Read lock attempt (`try_read`); never waits for the lock.
    TryRead,

    /// Write lock attempt (`try_write`); never waits for the lock.
    TryWrite,

    /// Releasing a read or write lock.
    Release,
}

#[derive(Debug)]
pub(super) struct State {
    /// A single `thread::Id` when Write locked.
    /// A map of `thread::Id` to read lock count when Read locked.
    lock: Option<Locked>,

    /// Tracks write access to the rwlock.
    last_access: Option<Access>,

    /// Tracks the last time the rwlock was released
    last_release_access: Option<Access>,

    /// Causality transfers between threads
    synchronize: Synchronize,
}

impl RwLock {
    /// Common RwLock function
    pub(crate) fn new() -> RwLock {
        super::execution(|execution| {
            let state = execution.objects.insert(State {
                lock: None,
                last_access: None,
                last_release_access: None,
                synchronize: Synchronize::new(),
            });

            RwLock { state }
        })
    }

    /// Acquire the read lock.
    /// Fail to acquire read lock if already *write* locked.
    pub(crate) fn acquire_read_lock(&self, location: Location) {
        self.state
            .branch_disable(Action::Read, self.is_write_locked(), location);

        assert!(
            self.post_acquire_read_lock(),
            "expected to be able to acquire read lock"
        );
    }

    /// Acquire write lock.
    /// Fail to acquire write lock if either read or write locked.
    pub(crate) fn acquire_write_lock(&self, location: Location) {
        self.state.branch_disable(
            Action::Write,
            self.is_write_locked() || self.is_read_locked(),
            location,
        );

        assert!(
            self.post_acquire_write_lock(),
            "expected to be able to acquire write lock"
        );
    }

    pub(crate) fn try_acquire_read_lock(&self, location: Location) -> bool {
        self.state.branch_action(Action::TryRead, location);
        self.post_acquire_read_lock()
    }

    pub(crate) fn try_acquire_write_lock(&self, location: Location) -> bool {
        self.state.branch_action(Action::TryWrite, location);
        self.post_acquire_write_lock()
    }

    /// Release a read lock, as a point at which other threads may be
    /// scheduled while the lock is still held. See `rt::Mutex::unlock`.
    pub(crate) fn unlock_read(&self, location: Location) {
        self.branch_release(location);
        self.release_read_lock();
    }

    /// Release the write lock, as a point at which other threads may be
    /// scheduled while the lock is still held. See `rt::Mutex::unlock`.
    pub(crate) fn unlock_write(&self, location: Location) {
        self.branch_release(location);
        self.release_write_lock();
    }

    /// Turn the write lock held by the current thread into a read lock,
    /// without letting a writer in between.
    pub(crate) fn downgrade(&self, location: Location) {
        // Like a release, this is a point at which other threads may run
        // while the write lock is still held.
        self.branch_release(location);

        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            let thread_id = execution.threads.active_id();

            assert!(
                state.lock == Some(Locked::Write(thread_id)),
                "[loom internal bug] downgrade without holding the write lock"
            );

            let mut readers = HashMap::new();
            readers.insert(thread_id, 1);
            state.lock = Some(Locked::Read(readers));

            // What was written under the lock is visible to the readers that
            // are let in now.
            state
                .synchronize
                .sync_store(&mut execution.threads, Release);

            // Establish sequential consistency between the lock's operations.
            execution.threads.seq_cst();

            // Readers no longer have to wait. Writers still do.
            for (id, thread) in execution.threads.iter_mut() {
                if id == thread_id {
                    continue;
                }

                if let Some(operation) = thread.operation.as_ref() {
                    if operation.object() == self.state.erase()
                        && operation.action() == Action::Read
                    {
                        thread.set_runnable();
                    }
                }
            }
        });
    }

    fn branch_release(&self, location: Location) {
        let can_branch = !std::thread::panicking()
            && super::execution(|execution| execution.threads.is_active());

        if can_branch {
            self.state.branch_action(Action::Release, location);
        }
    }

    pub(crate) fn release_read_lock(&self) {
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            let thread_id = execution.threads.active_id();

            state
                .synchronize
                .sync_store(&mut execution.threads, Release);

            // Establish sequential consistency between the lock's operations.
            execution.threads.seq_cst();

            let readers = match &mut state.lock {
                Some(Locked::Read(readers)) => readers,
                _ => panic!("invalid internal loom state"),
            };

            if let Some(count) = readers.get_mut(&thread_id) {
                *count -= 1;
                if *count == 0 {
                    readers.remove(&thread_id);
                }
            }

            if readers.is_empty() {
                state.lock = None;

                self.unlock_threads(execution, thread_id);
            }
        });
    }

    pub(crate) fn release_write_lock(&self) {
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            state.lock = None;

            state
                .synchronize
                .sync_store(&mut execution.threads, Release);

            // Establish sequential consistency between the lock's operations.
            execution.threads.seq_cst();

            let thread_id = execution.threads.active_id();

            self.unlock_threads(execution, thread_id);
        });
    }

    fn unlock_threads(&self, execution: &mut Execution, thread_id: thread::Id) {
        // TODO: This and the above function look very similar.
        // Refactor the two to DRY the code.
        for (id, thread) in execution.threads.iter_mut() {
            if id == thread_id {
                continue;
            }

            let obj = thread
                .operation
                .as_ref()
                .map(|operation| operation.object());

            if obj == Some(self.state.erase()) {
                thread.set_runnable();
            }
        }
    }

    /// Returns `true` if RwLock is read locked
    pub(crate) fn is_read_locked(&self) -> bool {
        super::execution(|execution| {
            let lock = &self.state.get(&execution.objects).lock;
            matches!(lock, Some(Locked::Read(_)))
        })
    }

    /// Returns `true` if RwLock is write locked.
    pub(crate) fn is_write_locked(&self) -> bool {
        super::execution(|execution| {
            let lock = &self.state.get(&execution.objects).lock;
            matches!(lock, Some(Locked::Write(_)))
        })
    }

    fn post_acquire_read_lock(&self) -> bool {
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            let thread_id = execution.threads.active_id();

            // Set the lock to the current thread
            let mut already_locked = false;
            state.lock = match state.lock.take() {
                None => {
                    let mut threads: HashMap<thread::Id, usize> = HashMap::new();
                    threads.insert(thread_id, 1);
                    Some(Locked::Read(threads))
                }
                Some(Locked::Read(mut threads)) => {
                    *threads.entry(thread_id).or_insert(0) += 1;
                    Some(Locked::Read(threads))
                }
                Some(Locked::Write(writer)) => {
                    already_locked = true;
                    Some(Locked::Write(writer))
                }
            };

            // The RwLock is already Write locked, so we cannot acquire a read lock on it.
            if already_locked {
                return false;
            }

            dbg!(state.synchronize.sync_load(&mut execution.threads, Acquire));

            execution.threads.seq_cst();

            // Block all writer threads from attempting to acquire the RwLock
            for (id, th) in execution.threads.iter_mut() {
                if id == thread_id {
                    continue;
                }

                let op = match th.operation.as_ref() {
                    Some(op) if op.object() == self.state.erase() => op,
                    _ => continue,
                };

                if op.action() == Action::Write {
                    let location = op.location();
                    th.set_blocked(location);
                }
            }

            true
        })
    }

    fn post_acquire_write_lock(&self) -> bool {
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            let thread_id = execution.threads.active_id();

            // Set the lock to the current thread
            state.lock = match state.lock {
                Some(Locked::Read(_)) => return false,
                Some(Locked::Write(_)) => return false,
                None => Some(Locked::Write(thread_id)),
            };

            state.synchronize.sync_load(&mut execution.threads, Acquire);

            // Establish sequential consistency between locks
            execution.threads.seq_cst();

            // Block all other threads waiting to acquire the RwLock. Threads
            // that are only attempting to (`try_read` / `try_write`) do not
            // wait; they will observe that the lock is held.
            for (id, th) in execution.threads.iter_mut() {
                if id == thread_id {
                    continue;
                }

                match th.operation.as_ref() {
                    Some(op)
                        if op.object() == self.state.erase()
                            && (op.action() == Action::Read || op.action() == Action::Write) =>
                    {
                        let location = op.location();
                        th.set_blocked(location);
                    }
                    _ => continue,
                };
            }

            true
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

    /// Returns `true` unless two operations that different threads are about
    /// to execute lead to the same state in either order.
    ///
    /// That is the case for two threads taking the lock for reading: both
    /// get it, or neither does, and each learns what the last writer knew.
    pub(super) fn dependent(&self, a: object::Action, b: object::Action) -> bool {
        let is_read = |action| action == Action::Read || action == Action::TryRead;

        !(is_read(a) && is_read(b))
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
        action == Action::TryRead || action == Action::TryWrite
    }

    fn is_release(action: object::Action) -> bool {
        action == Action::Release
    }
}
