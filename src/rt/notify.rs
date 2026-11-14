use crate::rt::object;
use crate::rt::{self, Access, Synchronize, VersionVec};

use std::sync::atomic::Ordering::{Acquire, Release};

use tracing::trace;

use super::Location;

#[derive(Debug, Copy, Clone)]
pub(crate) struct Notify {
    state: object::Ref<State>,
}

#[derive(Debug)]
pub(super) struct State {
    /// If true, spurious notifications are possible
    spurious: bool,

    /// True if the notify woke up spuriously last time
    did_spur: bool,

    /// When true, notification is sequential consistent.
    seq_cst: bool,

    /// `true` if there is a pending notification to consume.
    notified: bool,

    /// `true` if the object is only ever notified once, and waited for once
    /// per notification that is sent: a thread's exit.
    ///
    /// For such an object the wait and the notification commute. Whether the
    /// waiter gets there first and blocks, or finds the notification already
    /// there, the result is the same: the notification is consumed and the
    /// waiter continues after it. There is nothing to gain from exploring
    /// both orders, and with a `join` per thread in almost every model, a lot
    /// to lose.
    ///
    /// This does not hold in general. With several notifications, one that
    /// arrives while another is still pending is absorbed, so it matters
    /// whether a wait comes in between.
    one_shot: bool,

    /// Tracks access to the notify object
    last_access: Option<Access>,

    /// Causality transfers between threads
    synchronize: Synchronize,
}

impl Notify {
    pub(crate) fn new(seq_cst: bool, spurious: bool) -> Notify {
        Self::with(seq_cst, spurious, false)
    }

    /// A notification that is sent exactly once, such as a thread's exit.
    pub(crate) fn one_shot() -> Notify {
        Self::with(true, false, true)
    }

    fn with(seq_cst: bool, spurious: bool, one_shot: bool) -> Notify {
        super::execution(|execution| {
            let state = execution.objects.insert(State {
                spurious,
                did_spur: false,
                seq_cst,
                notified: false,
                one_shot,
                last_access: None,
                synchronize: Synchronize::new(),
            });

            trace!(?state, ?seq_cst, ?spurious, "Notify::new");

            Notify { state }
        })
    }

    pub(crate) fn notify(self, location: Location) {
        self.state.branch_opaque(location);

        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            state
                .synchronize
                .sync_store(&mut execution.threads, Release);

            if state.seq_cst {
                execution.threads.seq_cst();
            }

            state.notified = true;

            let (active, inactive) = execution.threads.split_active();

            for thread in inactive {
                let obj = thread
                    .operation
                    .as_ref()
                    .map(|operation| operation.object());

                if obj == Some(self.state.erase()) {
                    trace!(state = ?self.state, thread = ?thread.id, %location, "Notify::notify");

                    thread.wake(active);
                }
            }
        });
    }

    /// Returns `true` if there is a pending notification, without consuming
    /// it.
    ///
    /// This is a branch point that is dependent with `notify`, so both
    /// orderings of the check and a racing notification get explored.
    pub(crate) fn is_notified(self, location: Location) -> bool {
        self.state.branch_opaque(location);

        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            trace!(state = ?self.state, notified = ?state.notified, %location, "Notify::is_notified");

            if state.notified {
                // Observing the notification establishes causality with the
                // notifier, as consuming it would.
                state.synchronize.sync_load(&mut execution.threads, Acquire);

                if state.seq_cst {
                    execution.threads.seq_cst();
                }
            }

            state.notified
        })
    }

    pub(crate) fn wait(self, location: Location) {
        let (notified, spurious) = rt::execution(|execution| {
            let spurious = if self.state.get(&execution.objects).might_spur() {
                execution.path.branch_spurious()
            } else {
                false
            };

            let state = self.state.get_mut(&mut execution.objects);

            if spurious {
                state.did_spur = true;
            }

            trace!(state = ?self.state, notified = ?state.notified, ?spurious, %location, "Notify::wait 1");
            dbg!((state.notified, spurious))
        });

        if spurious {
            rt::yield_now();
            return;
        }

        self.state
            .branch_disable(object::Action::Wait, !notified, location);

        // Thread was notified
        super::execution(|execution| {
            trace!(state = ?self.state, %location, "Notify::wait 2");
            let state = self.state.get_mut(&mut execution.objects);

            assert!(state.notified);

            state.synchronize.sync_load(&mut execution.threads, Acquire);

            if state.seq_cst {
                // Establish sequential consistency between locks
                execution.threads.seq_cst();
            }

            state.notified = false;
        });
    }
}

impl State {
    pub(crate) fn might_spur(&self) -> bool {
        self.spurious && !self.did_spur
    }

    /// Returns `true` if `action` is a wait that commutes with the
    /// notification it waits for.
    fn is_one_shot_wait(&self, action: object::Action) -> bool {
        self.one_shot && action == object::Action::Wait
    }

    /// Returns the previous access that `action` is dependent with.
    ///
    /// With `racing` set, this leaves out an access that `action` comes
    /// after but cannot be reordered with. A wait for a one-shot
    /// notification is ordered after the notification, which matters for
    /// what the waiting thread does next: all of that comes after everything
    /// the notifying thread did. But there is no execution to be found by
    /// trying the wait first.
    pub(crate) fn last_dependent_access(
        &self,
        action: object::Action,
        racing: bool,
    ) -> Option<&Access> {
        if racing && self.is_one_shot_wait(action) {
            return None;
        }

        self.last_access.as_ref()
    }

    pub(crate) fn set_last_access(
        &mut self,
        action: object::Action,
        path_id: usize,
        version: &VersionVec,
    ) {
        if self.is_one_shot_wait(action) {
            // Nothing is to be reordered with the wait either.
            return;
        }

        Access::set_or_create(&mut self.last_access, path_id, version);
    }
}
