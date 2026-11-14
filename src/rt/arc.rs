use crate::rt::object;
use crate::rt::{self, Access, Location, Synchronize, VersionVec, MAX_THREADS};

use std::sync::atomic::Ordering::{Acquire, Release, SeqCst};

use tracing::trace;
#[derive(Debug)]
pub(crate) struct Arc {
    state: object::Ref<State>,
}

#[derive(Debug)]
pub(super) struct State {
    /// Reference count
    ref_cnt: usize,

    /// Location where the arc was allocated
    allocated: Location,

    /// Causality transfers between threads
    ///
    /// Only updated on on ref dec and acquired before drop
    synchronize: Synchronize,

    /// Tracks access to the arc object, for deciding which operations to
    /// explore in the other order. One access per thread and kind, as none
    /// of the kinds is ordered among itself:
    ///
    /// - Clones commute with each other and with drops. A thread can only
    ///   clone or drop a handle of its own, so neither can make the other
    ///   impossible.
    /// - Drops commute with each other as well, with one exception: the drop
    ///   that brings the count to zero frees the value, and runs its
    ///   destructor on that thread. Which handle is the last to go is the
    ///   only thing about the order of drops that can be observed, so the
    ///   drop that frees the value is dependent with the last drop of every
    ///   other thread, and no other drop is dependent with any.
    /// - An inspection of the count is dependent with every change of it,
    ///   and not with other inspections.
    last_ref_inc: [Option<Access>; MAX_THREADS],
    last_ref_dec: [Option<Access>; MAX_THREADS],
    last_ref_inspect: [Option<Access>; MAX_THREADS],
}

/// Actions performed on the Arc
///
/// Clones are only dependent with inspections. Drops are dependent with
/// inspections, and the drop that frees the value with the other drops.
#[derive(Debug, Copy, Clone, PartialEq)]
pub(super) enum Action {
    /// Clone the arc
    RefInc,

    /// Drop the Arc
    RefDec,

    /// Inspect internals (such as get ref count). This is done with SeqCst
    /// causality
    Inspect,
}

impl Arc {
    pub(crate) fn new(location: Location) -> Arc {
        rt::execution(|execution| {
            let state = execution.objects.insert(State {
                ref_cnt: 1,
                allocated: location,
                synchronize: Synchronize::new(),
                last_ref_inc: Default::default(),
                last_ref_dec: Default::default(),
                last_ref_inspect: Default::default(),
            });

            trace!(?state, %location, "Arc::new");

            Arc { state }
        })
    }

    pub(crate) fn ref_inc(&self, location: Location) {
        self.branch(Action::RefInc, location);

        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            state.ref_cnt = state.ref_cnt.checked_add(1).expect("overflow");

            trace!(state = ?self.state, ref_cnt = ?state.ref_cnt, %location, "Arc::ref_inc");
        })
    }

    /// Validate a `get_mut` call
    pub(crate) fn get_mut(&self, location: Location) -> bool {
        // Whether this is the only handle depends on what the other handles'
        // owners have done so far.
        self.branch(Action::Inspect, location);

        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            assert!(state.ref_cnt >= 1, "Arc is released");

            // Synchronize the threads
            state.synchronize.sync_load(&mut execution.threads, Acquire);

            let is_only_ref = state.ref_cnt == 1;

            trace!(state = ?self.state, ?is_only_ref, %location, "Arc::get_mut");

            is_only_ref
        })
    }

    /// Returns true if the memory should be dropped.
    pub(crate) fn ref_dec(&self, location: Location) -> bool {
        self.branch(Action::RefDec, location);

        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            assert!(state.ref_cnt >= 1, "Arc is already released");

            // Decrement the ref count
            state.ref_cnt -= 1;

            trace!(state = ?self.state, ref_cnt = ?state.ref_cnt, %location, "Arc::ref_dec");

            // Synchronize the threads.
            state
                .synchronize
                .sync_store(&mut execution.threads, Release);

            if state.ref_cnt == 0 {
                // Final ref count, the arc will be dropped. This requires
                // acquiring the causality
                //
                // In the real implementation, this is done with a fence.
                state.synchronize.sync_load(&mut execution.threads, Acquire);
                true
            } else {
                false
            }
        })
    }

    #[track_caller]
    pub(crate) fn strong_count(&self) -> usize {
        self.branch(Action::Inspect, location!());

        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            assert!(state.ref_cnt > 0, "Arc is already released");

            // Synchronize the threads.
            state.synchronize.sync_load(&mut execution.threads, SeqCst);

            state.ref_cnt
        })
    }

    fn branch(&self, action: Action, location: Location) {
        let r = self.state;
        r.branch_action(action, location);
        assert!(
            r.ref_eq(self.state),
            "Internal state mutated during branch. This is \
                usually due to a bug in the algorithm being tested writing in \
                an invalid memory location."
        );
    }
}

impl State {
    pub(super) fn check_for_leaks(&self, index: usize) {
        if self.ref_cnt != 0 {
            if self.allocated.is_captured() {
                panic!(
                    "Arc leaked.\n  Allocated: {}\n      Index: {}",
                    self.allocated, index
                );
            } else {
                panic!("Arc leaked.\n  Index: {}", index);
            }
        }
    }

    /// Returns `true` if the next drop of a handle is the one that frees the
    /// value.
    fn next_drop_frees(&self) -> bool {
        self.ref_cnt == 1
    }

    /// Returns the previous accesses that `action` is dependent with.
    pub(super) fn dependent_accesses(&self, action: Action) -> impl Iterator<Item = &Access> {
        let (inspections, incs, decs) = match action {
            // Clones are only dependent with inspections.
            Action::RefInc => (&self.last_ref_inspect[..], &[][..], &[][..]),
            // Drops are dependent with inspections. The one that frees the
            // value is dependent with the other drops, too.
            Action::RefDec if self.next_drop_frees() => {
                (&self.last_ref_inspect[..], &[][..], &self.last_ref_dec[..])
            }
            Action::RefDec => (&self.last_ref_inspect[..], &[][..], &[][..]),
            // Inspections are dependent with everything that changes the
            // count.
            Action::Inspect => (&[][..], &self.last_ref_inc[..], &self.last_ref_dec[..]),
        };

        inspections
            .iter()
            .chain(incs.iter())
            .chain(decs.iter())
            .flatten()
    }

    /// Returns `true` if `action` has to be tried before each of the
    /// accesses it is dependent with, and not only before the most recent
    /// one.
    ///
    /// Trying it before the most recent one is enough when doing so turns
    /// the access before that into the most recent one of the next
    /// execution. That is not the case for the drop that frees the value:
    /// moved before another drop, it is not the one that frees the value
    /// anymore, and has nothing left to be reordered with. Each of the
    /// other handles has to get its turn at being the last one directly.
    pub(super) fn races_with_all(&self, action: Action) -> bool {
        action == Action::RefDec && self.next_drop_frees()
    }

    /// Returns `true` unless two operations by different threads lead to the
    /// same state in either order.
    pub(super) fn dependent(&self, a: Action, b: Action) -> bool {
        use Action::*;

        match (a, b) {
            // Clones commute with each other and with drops.
            (RefInc, RefInc) | (RefInc, RefDec) | (RefDec, RefInc) => false,
            // Drops commute unless the second one frees the value, in which
            // case the order decides which thread that is.
            (RefDec, RefDec) => self.ref_cnt <= 2,
            // Inspections do not affect each other.
            (Inspect, Inspect) => false,
            // An inspection sees the count change.
            _ => true,
        }
    }

    pub(super) fn set_last_access(
        &mut self,
        action: Action,
        thread: usize,
        path_id: usize,
        version: &VersionVec,
    ) {
        let access = match action {
            Action::RefInc => &mut self.last_ref_inc[thread],
            Action::RefDec => &mut self.last_ref_dec[thread],
            Action::Inspect => &mut self.last_ref_inspect[thread],
        };

        Access::set_or_create(access, path_id, version)
    }
}
