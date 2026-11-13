//! An atomic cell
//!
//! See the CDSChecker paper for detailed explanation.
//!
//! # Modification order implications (figure 7)
//!
//! - Read-Read Coherence:
//!
//!   On `load`, all stores are iterated, finding stores that were read by
//!   actions in the current thread's causality. These loads happen-before the
//!   current load. The `modification_order` of these happen-before loads are
//!   joined into the current load's `modification_order`.
//!
//! - Write-Read Coherence:
//!
//!   On `load`, all stores are iterated, finding stores that happens-before the
//!   current thread's causality. The `modification_order` of these stores are
//!   joined into the current load's `modification_order`.
//!
//! - Read-Write Coherence:
//!
//!   On `store`, find all existing stores that were read in the current
//!   thread's causality. Join these stores' `modification_order` into the new
//!   store's modification order.
//!
//! - Write-Write Coherence:
//!
//!   The `modification_order` is initialized to the thread's causality. Any
//!   store that happened in the thread causality will be earlier in the
//!   modification order.
//!
//! - Seq-cst/MO Consistency:
//!
//! - Seq-cst Write-Read Coherence:
//!
//! - RMW/MO Consistency: Subsumed by Write-Write Coherence?
//!
//! - RMW Atomicity:
//!
//!
//! # Fence modification order implications (figure 9)
//!
//! - SC Fences Restrict RF:
//! - SC Fences Restrict RF (Collapsed Store):
//! - SC Fences Restrict RF (Collapsed Load):
//! - SC Fences Impose MO:
//! - SC Fences Impose MO (Collapsed 1st Store):
//! - SC Fences Impose MO (Collapsed 2st Store):
//!
//!
//! # Fence Synchronization implications (figure 10)
//!
//! - Fence Synchronization
//! - Fence Synchronization (Collapsed Store)
//! - Fence Synchronization (Collapsed Load)

use crate::rt::execution::Execution;
use crate::rt::location::{self, Location, LocationSet};
use crate::rt::object;
use crate::rt::{
    self, thread, Access, Numeric, Synchronize, VersionVec, MAX_ATOMIC_HISTORY, MAX_THREADS,
};

use std::cmp;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;

use tracing::trace;

#[derive(Debug)]
pub(crate) struct Atomic<T> {
    state: object::Ref<State>,
    _p: PhantomData<fn() -> T>,
}

#[derive(Debug)]
pub(super) struct State {
    /// Where the atomic was created
    created_location: Location,

    /// Transitive closure of all atomic loads from the cell.
    loaded_at: VersionVec,

    /// Location for the *last* time a thread atomically loaded from the cell.
    loaded_locations: LocationSet,

    /// Transitive closure of all **unsynchronized** loads from the cell.
    unsync_loaded_at: VersionVec,

    /// Location for the *last* time a thread read **synchronized** from the cell.
    unsync_loaded_locations: LocationSet,

    /// Transitive closure of all atomic stores to the cell.
    stored_at: VersionVec,

    /// Location for the *last* time a thread atomically stored to the cell.
    stored_locations: LocationSet,

    /// Version of the most recent **unsynchronized** mutable access to the
    /// cell.
    ///
    /// This includes the initialization of the cell as well as any calls to
    /// `get_mut`.
    unsync_mut_at: VersionVec,

    /// Location for the *last* time a thread `with_mut` from the cell.
    unsync_mut_locations: LocationSet,

    /// `true` when in a `with_mut` closure. If this is set, there can be no
    /// access to the cell.
    is_mutating: bool,

    /// Last time each thread loaded from the atomic since the last store or
    /// rmw operation. Together with `last_non_load_access`, this tracks the
    /// dependent accesses for the DPOR algorithm.
    ///
    /// Loads are independent of each other, so the most recent one says
    /// nothing about the ones before it: a store races with the last load of
    /// *every* thread it is not ordered after. Keeping only a single "last
    /// access" lets one thread's load hide another thread's earlier load,
    /// and the interleaving that reorders the store before that hidden load
    /// is then never explored.
    last_load_access: [Option<Access>; MAX_THREADS],

    /// Last `SeqCst` load. `SeqCst` loads are ordered among themselves (a
    /// later one cannot return an older value than an earlier one did), so
    /// unlike other loads they are dependent with each other.
    last_sc_load_access: Option<Access>,

    /// Last time the atomic was accessed for a store or rmw operation. Every
    /// access before it is ordered before it, so nothing older needs to be
    /// remembered.
    last_non_load_access: Option<Access>,

    /// Last time the atomic was accessed using an RMW access that did not fail.
    ///
    /// This is used for applying the RMW atomicity constraint to other stores
    /// (so they don't squeeze in between the read and write part of the RMW).
    last_successful_rmw: VersionVec,

    /// How far the `SeqCst` order has got on this atomic, as a bit set of
    /// slots in `stores`: the stores written by `SeqCst` stores, returned by
    /// `SeqCst` loads, or known to a thread when it executed a `SeqCst`
    /// fence. `SeqCst` operations execute in the order they have in the
    /// `SeqCst` total order, so every later `SeqCst` operation on this atomic
    /// has to be consistent with these stores coming first:
    ///
    /// - a `SeqCst` store is ordered after all of them, and
    /// - a `SeqCst` load does not return a store ordered before any of them.
    sc_floor: u8,

    /// `sc_floor` as of each thread's last `SeqCst` fence. Stores made by the
    /// thread after the fence, whatever their ordering, come after these.
    fence_floor: [u8; MAX_THREADS],

    /// Currently tracked stored values. This is the `MAX_ATOMIC_HISTORY` most
    /// recent stores to the atomic cell in loom execution order.
    stores: [Store; MAX_ATOMIC_HISTORY],

    /// The total number of stores to the cell.
    cnt: u16,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub(super) enum Action {
    /// Atomic load
    Load,

    /// Atomic store
    Store,

    /// Atomic read-modify-write
    Rmw,

    /// Atomic load with `SeqCst` ordering
    LoadSeqCst,

    /// Atomic store with `SeqCst` ordering
    StoreSeqCst,

    /// Atomic read-modify-write with a `SeqCst` ordering
    RmwSeqCst,
}

impl Action {
    fn load(ordering: Ordering) -> Action {
        if is_seq_cst(ordering) {
            Action::LoadSeqCst
        } else {
            Action::Load
        }
    }

    fn store(ordering: Ordering) -> Action {
        if is_seq_cst(ordering) {
            Action::StoreSeqCst
        } else {
            Action::Store
        }
    }

    fn rmw(success: Ordering, failure: Ordering) -> Action {
        if is_seq_cst(success) || is_seq_cst(failure) {
            Action::RmwSeqCst
        } else {
            Action::Rmw
        }
    }

    fn is_load(self) -> bool {
        matches!(self, Action::Load | Action::LoadSeqCst)
    }

    /// Returns `true` if the access takes part in the `SeqCst` order.
    pub(super) fn is_seq_cst(self) -> bool {
        matches!(
            self,
            Action::LoadSeqCst | Action::StoreSeqCst | Action::RmwSeqCst
        )
    }
}

#[derive(Debug)]
struct Store {
    /// The stored value. All atomic types can be converted to `u64`.
    value: u64,

    /// The causality of the thread when it stores the value.
    happens_before: VersionVec,

    /// Tracks the modification order. Order is tracked as a partially-ordered
    /// set.
    modification_order: VersionVec,

    /// Manages causality transfers between threads
    sync: Synchronize,

    /// Tracks when each thread first saw value
    first_seen: FirstSeen,

    /// True when the store was done with `SeqCst` ordering
    seq_cst: bool,

    /// Stores (as a bit set of slots in `State::stores`) that are ordered
    /// before this one in the modification order because of the `SeqCst`
    /// order, in addition to what `modification_order` says.
    ///
    /// A `SeqCst` load that returns this store places it after everything
    /// the `SeqCst` order has already been through. The store exists by
    /// then, with other stores possibly ordered after it, so that cannot be
    /// expressed by changing its version vector.
    mo_preds: u8,

    /// `true` once the store is behind the `SeqCst` order: some store that
    /// is part of that order comes after it in the modification order. A
    /// `SeqCst` load can no longer return it.
    sc_stale: bool,

    /// The threads (as a bit set) for which the store was behind the `SeqCst`
    /// order when they last executed a `SeqCst` fence. None of their loads
    /// can return it anymore.
    fence_stale: u8,
}

#[derive(Debug)]
struct FirstSeen([u16; MAX_THREADS]);

/// The order of `SeqCst` fences.
///
/// `SeqCst` fences are totally ordered, and that order is observable: a fence
/// sees what the fences before it published, and not what the ones after it
/// did. This object exists so that fences are operations the scheduler knows
/// to be dependent on one another, and explores in both orders. There is one
/// per execution, created by the first `SeqCst` fence.
#[derive(Debug)]
pub(super) struct SeqCstFences {
    /// The last `SeqCst` fence.
    last_access: Option<Access>,

    /// The last `SeqCst` load, store or rmw of each thread, on any atomic.
    /// A fence is ordered with respect to those as well: it sees the `SeqCst`
    /// stores before it, and what its thread knows constrains the `SeqCst`
    /// loads and stores after it.
    last_sc_access: [Option<Access>; MAX_THREADS],
}

impl SeqCstFences {
    /// Returns the object for the current execution, creating it if this is
    /// the first operation that takes part in the `SeqCst` order.
    fn get(execution: &mut Execution) -> object::Ref<SeqCstFences> {
        execution.objects.seq_cst_fences().unwrap_or_else(|| {
            let fences = execution.objects.insert(SeqCstFences {
                last_access: None,
                last_sc_access: Default::default(),
            });

            execution.objects.set_seq_cst_fences(fences);
            fences
        })
    }

    /// The previous accesses a fence is dependent with.
    pub(super) fn dependent_accesses(&self) -> impl Iterator<Item = &Access> {
        self.last_access
            .iter()
            .chain(self.last_sc_access.iter().flatten())
    }

    /// The last fence, which `SeqCst` accesses are dependent with.
    pub(super) fn last_fence_access(&self) -> Option<&Access> {
        self.last_access.as_ref()
    }

    /// Records a fence.
    pub(super) fn set_last_access(&mut self, path_id: usize, version: &VersionVec) {
        Access::set_or_create(&mut self.last_access, path_id, version);

        // The fence is ordered after all of these.
        self.last_sc_access = Default::default();
    }

    /// Records a `SeqCst` access to some atomic.
    pub(super) fn set_last_sc_access(
        &mut self,
        thread: usize,
        path_id: usize,
        version: &VersionVec,
    ) {
        Access::set_or_create(&mut self.last_sc_access[thread], path_id, version);
    }
}

/// Implements atomic fence behavior
pub(crate) fn fence(ordering: Ordering, location: Location) {
    if let Ordering::SeqCst = ordering {
        let fences = rt::execution(SeqCstFences::get);

        fences.branch_opaque(location);
    }

    rt::synchronize(|execution| match ordering {
        Ordering::Acquire => fence_acq(execution),
        Ordering::Release => fence_rel(execution),
        Ordering::AcqRel => fence_acqrel(execution),
        Ordering::SeqCst => fence_seqcst(execution),
        Ordering::Relaxed => panic!("there is no such thing as a relaxed fence"),
        order => unimplemented!("unimplemented ordering {:?}", order),
    });
}

fn fence_acq(execution: &mut Execution) {
    // Find all stores for all atomic objects and, if they have been read by
    // the current thread, establish an acquire synchronization.
    for state in execution.objects.iter_mut::<State>() {
        // Iterate all the stores
        for store in state.stores_mut() {
            if !store.first_seen.is_seen_by_current(&execution.threads) {
                continue;
            }

            store
                .sync
                .sync_load(&mut execution.threads, Ordering::Acquire);
        }
    }
}

fn fence_rel(execution: &mut Execution) {
    // take snapshot of cur view and record as rel view
    let active = execution.threads.active_mut();
    active.released = active.causality;
}

fn fence_acqrel(execution: &mut Execution) {
    fence_acq(execution);
    fence_rel(execution);
}

fn fence_seqcst(execution: &mut Execution) {
    fence_acqrel(execution);
    execution.threads.seq_cst_fence();

    // Order the fence with the `SeqCst` loads and stores.
    for state in execution.objects.iter_mut::<State>() {
        state.sc_fence(&execution.threads);
    }
}

impl<T: Numeric> Atomic<T> {
    /// Create a new, atomic cell initialized with the provided value
    pub(crate) fn new(value: T, location: Location) -> Atomic<T> {
        rt::execution(|execution| {
            let state = State::new(&mut execution.threads, value.into_u64(), location);
            let state = execution.objects.insert(state);

            trace!(?state, %location, "Atomic::new");

            Atomic {
                state,
                _p: PhantomData,
            }
        })
    }

    /// Loads a value from the atomic cell.
    pub(crate) fn load(&self, location: Location, ordering: Ordering) -> T {
        self.branch(Action::load(ordering), location);

        super::synchronize(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            // If necessary, generate the list of stores to permute through
            if execution.path.is_traversed() {
                let mut seed = [0; MAX_ATOMIC_HISTORY];

                let n = state.match_load_to_stores(&execution.threads, &mut seed[..], ordering);

                execution.path.push_load(&seed[..n]);
            }

            // Get the store to return from this load.
            let index = execution.path.branch_load();

            trace!(state = ?self.state, ?ordering, %location, "Atomic::load");

            T::from_u64(state.load(&mut execution.threads, index, location, ordering))
        })
    }

    /// Loads a value from the atomic cell without performing synchronization
    pub(crate) fn unsync_load(&self, location: Location) -> T {
        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            state
                .unsync_loaded_locations
                .track(location, &execution.threads);

            // An unsync load counts as a "read" access
            state.track_unsync_load(&execution.threads);

            trace!(state = ?self.state, %location, "Atomic::unsync_load");

            // Return the value
            let index = index(state.cnt - 1);
            T::from_u64(state.stores[index].value)
        })
    }

    /// Stores a value into the atomic cell.
    pub(crate) fn store(&self, location: Location, val: T, ordering: Ordering) {
        self.branch(Action::store(ordering), location);

        super::synchronize(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            state.stored_locations.track(location, &execution.threads);

            // An atomic store counts as a read access to the underlying memory
            // cell.
            state.track_store(&execution.threads);

            trace!(state = ?self.state, ?ordering, %location, "Atomic::store");

            // Do the store
            state.store(
                &mut execution.threads,
                Synchronize::new(),
                val.into_u64(),
                ordering,
            );
        })
    }

    pub(crate) fn rmw<F, E>(
        &self,
        location: Location,
        success: Ordering,
        failure: Ordering,
        f: F,
    ) -> Result<T, E>
    where
        F: FnOnce(T) -> Result<T, E>,
    {
        self.branch(Action::rmw(success, failure), location);

        super::synchronize(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            // If necessary, generate the list of stores to permute through
            if execution.path.is_traversed() {
                let mut seed = [0; MAX_ATOMIC_HISTORY];

                let n = state.match_rmw_to_stores(&mut seed[..]);
                execution.path.push_load(&seed[..n]);
            }

            // Get the store to use for the read portion of the rmw operation.
            let index = execution.path.branch_load();

            trace!(state = ?self.state, ?success, ?failure, %location, "Atomic::rmw");

            state
                .rmw(
                    &mut execution.threads,
                    index,
                    location,
                    success,
                    failure,
                    |num| f(T::from_u64(num)).map(T::into_u64),
                )
                .map(T::from_u64)
        })
    }

    /// Access a mutable reference to value most recently stored.
    ///
    /// `with_mut` must happen-after all stores to the cell.
    pub(crate) fn with_mut<R>(&mut self, location: Location, f: impl FnOnce(&mut T) -> R) -> R {
        let value = super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            state
                .unsync_mut_locations
                .track(location, &execution.threads);
            // Verify the mutation may happen
            state.track_unsync_mut(&execution.threads);
            state.is_mutating = true;

            trace!(state = ?self.state, %location, "Atomic::with_mut");

            // Return the value of the most recent store
            let index = index(state.cnt - 1);
            T::from_u64(state.stores[index].value)
        });

        struct Reset<T: Numeric>(T, object::Ref<State>);

        impl<T: Numeric> Drop for Reset<T> {
            fn drop(&mut self) {
                super::execution(|execution| {
                    let state = self.1.get_mut(&mut execution.objects);

                    // Make sure the state is as expected
                    assert!(state.is_mutating);
                    state.is_mutating = false;

                    // The value may have been mutated, so it must be placed
                    // back.
                    let index = index(state.cnt - 1);
                    state.stores[index].value = T::into_u64(self.0);

                    if !std::thread::panicking() {
                        state.track_unsync_mut(&execution.threads);
                    }
                });
            }
        }

        // Unset on exit
        let mut reset = Reset(value, self.state);
        f(&mut reset.0)
    }

    fn branch(&self, action: Action, location: Location) {
        if action.is_seq_cst() {
            // The access is ordered with respect to `SeqCst` fences.
            rt::execution(SeqCstFences::get);
        }

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

// ===== impl State =====

impl State {
    fn new(threads: &mut thread::Set, value: u64, location: Location) -> State {
        let mut state = State {
            created_location: location,
            loaded_at: VersionVec::new(),
            loaded_locations: LocationSet::new(),
            unsync_loaded_at: VersionVec::new(),
            unsync_loaded_locations: LocationSet::new(),
            stored_at: VersionVec::new(),
            stored_locations: LocationSet::new(),
            unsync_mut_at: VersionVec::new(),
            unsync_mut_locations: LocationSet::new(),
            is_mutating: false,
            last_load_access: Default::default(),
            last_sc_load_access: None,
            last_non_load_access: None,
            sc_floor: 0,
            fence_floor: [0; MAX_THREADS],
            last_successful_rmw: VersionVec::new(),
            stores: Default::default(),
            cnt: 0,
        };

        // All subsequent accesses must happen-after. Creating the atomic is
        // its first unsynchronized mutable access; remember where it happened
        // so that an access racing with the creation can be reported with a
        // location.
        state.unsync_mut_locations.track(location, threads);
        state.track_unsync_mut(threads);

        // Store the initial thread
        //
        // The actual order shouldn't matter as operation on the atomic
        // **should** already include the thread causality resulting in the
        // creation of this atomic cell.
        //
        // This is verified using `cell`.
        state.store(threads, Synchronize::new(), value, Ordering::Release);

        state
    }

    fn load(
        &mut self,
        threads: &mut thread::Set,
        index: usize,
        location: Location,
        ordering: Ordering,
    ) -> u64 {
        self.loaded_locations.track(location, threads);
        // Validate memory safety
        self.track_load(threads);

        // Apply coherence rules
        self.apply_load_coherence(threads, index);

        if is_seq_cst(ordering) {
            self.sc_load(index);
        }

        let store = &mut self.stores[index];

        store.first_seen.touch(threads);
        store.sync.sync_load(threads, ordering);
        store.value
    }

    fn store(
        &mut self,
        threads: &mut thread::Set,
        mut sync: Synchronize,
        value: u64,
        ordering: Ordering,
    ) {
        let index = index(self.cnt);

        // Increment the count
        self.cnt += 1;

        // The modification order is initialized to the thread's current
        // causality. All reads / writes that happen before this store are
        // ordered before the store.
        let happens_before = threads.active().causality;

        // Starting with the thread's causality covers WRITE-WRITE coherence
        let mut modification_order = happens_before;

        // Apply coherence rules
        for i in 0..self.stores.len() {
            // READ-WRITE coherence
            if self.stores[i].first_seen.is_seen_by_current(threads) {
                let mo = self.stores[i].modification_order;
                modification_order.join(&mo);
            }
        }

        // RMW Atomicity.
        modification_order.join(&self.last_successful_rmw);

        // The slot is about to be reused; forget what was known about the
        // store it held.
        self.forget_slot(index);

        // A `SeqCst` store comes after everything the `SeqCst` order has been
        // through. Any store comes after what that order had been through
        // when the thread last executed a `SeqCst` fence.
        let mut floor = self.fence_floor[threads.active_id().as_usize()];

        if is_seq_cst(ordering) {
            floor |= self.sc_floor;
        }

        for i in slots(floor) {
            let mo = self.stores[i].modification_order;
            modification_order.join(&mo);
        }

        sync.sync_store(threads, ordering);

        let mut first_seen = FirstSeen::new();
        first_seen.touch(threads);

        // Track the store
        self.stores[index] = Store {
            value,
            happens_before,
            modification_order,
            sync,
            first_seen,
            seq_cst: is_seq_cst(ordering),
            mo_preds: 0,
            sc_stale: false,
            fence_stale: 0,
        };

        if is_seq_cst(ordering) {
            self.sc_advance(index);
        }
    }

    /// The modification order as far as it is known: bit `i` of entry `j` is
    /// set if the store in slot `i` is ordered before the one in slot `j`.
    fn mo_before(&self) -> [u8; MAX_ATOMIC_HISTORY] {
        let cnt = cmp::min(self.cnt as usize, MAX_ATOMIC_HISTORY);
        let mut before = [0u8; MAX_ATOMIC_HISTORY];

        for j in 0..cnt {
            before[j] = self.stores[j].mo_preds & !(1 << j);

            for i in 0..cnt {
                if i != j && self.stores[i].modification_order < self.stores[j].modification_order {
                    before[j] |= 1 << i;
                }
            }
        }

        // Transitive closure
        for k in 0..cnt {
            for j in 0..cnt {
                if before[j] & (1 << k) != 0 {
                    before[j] |= before[k];
                }
            }
        }

        before
    }

    /// Returns `true` if a `SeqCst` load can no longer return the store in
    /// `slot`.
    fn is_sc_stale(&self, slot: usize, before: &[u8; MAX_ATOMIC_HISTORY]) -> bool {
        self.stores[slot].sc_stale || slots(self.sc_floor).any(|f| before[f] & (1 << slot) != 0)
    }

    /// The store in `slot` becomes part of the `SeqCst` order.
    fn sc_advance(&mut self, slot: usize) {
        self.sc_floor |= 1 << slot;

        let before = self.mo_before();

        for i in slots(before[slot]) {
            self.stores[i].sc_stale = true;
        }
    }

    /// A `SeqCst` load returns the store in `slot`.
    fn sc_load(&mut self, slot: usize) {
        let before = self.mo_before();

        debug_assert!(
            !self.is_sc_stale(slot, &before),
            "[loom internal bug] SeqCst load of a stale store"
        );

        // The store is not ordered before anything the `SeqCst` order has
        // been through (it would be stale), so it comes after all of it.
        self.stores[slot].mo_preds |= self.sc_floor & !(1 << slot) & !before[slot];

        self.sc_advance(slot);
    }

    /// A `SeqCst` fence executed by the current thread.
    fn sc_fence(&mut self, threads: &thread::Set) {
        let cnt = cmp::min(self.cnt as usize, MAX_ATOMIC_HISTORY);
        let thread = threads.active_id().as_usize();

        // What the thread knows about is, from here on, behind every later
        // `SeqCst` operation.
        for i in 0..cnt {
            let store = &self.stores[i];

            if store.happens_before <= threads.active().causality
                || store.first_seen.is_seen_by_current(threads)
            {
                self.sc_floor |= 1 << i;
            }
        }

        let before = self.mo_before();

        for f in slots(self.sc_floor) {
            for i in slots(before[f]) {
                self.stores[i].sc_stale = true;
            }
        }

        // And the thread itself is, from here on, past everything the
        // `SeqCst` order has been through.
        for store in &mut self.stores[..cnt] {
            if store.sc_stale {
                store.fence_stale |= 1 << thread;
            }
        }

        self.fence_floor[thread] = self.sc_floor;
    }

    fn forget_slot(&mut self, slot: usize) {
        let mask = !(1u8 << slot);

        self.sc_floor &= mask;

        for floor in &mut self.fence_floor {
            *floor &= mask;
        }

        for store in &mut self.stores {
            store.mo_preds &= mask;
        }
    }

    fn rmw<E>(
        &mut self,
        threads: &mut thread::Set,
        index: usize,
        location: Location,
        success: Ordering,
        failure: Ordering,
        f: impl FnOnce(u64) -> Result<u64, E>,
    ) -> Result<u64, E> {
        self.loaded_locations.track(location, threads);

        // Track the load is happening in order to ensure correct
        // synchronization to the underlying cell.
        self.track_load(threads);

        // Apply coherence rules.
        self.apply_load_coherence(threads, index);

        self.stores[index].first_seen.touch(threads);

        let prev = self.stores[index].value;

        match f(prev) {
            Ok(next) => {
                if is_seq_cst(success) {
                    self.sc_load(index);
                }

                // Perform load synchronization using the `success` ordering.
                //
                // This has to come before the store is validated: the store
                // half of the RMW happens after everything the load half
                // synchronizes with. Checking it against the thread's
                // causality from before the load reports accesses that the
                // load orders before this store as concurrent with it.
                self.stores[index].sync.sync_load(threads, success);

                self.stored_locations.track(location, threads);
                // Track a store operation happened
                self.track_store(threads);

                // Store the new value, initializing with the `sync` value from
                // the load. This is our (hacky) way to establish a release
                // sequence.
                let sync = self.stores[index].sync;
                self.store(threads, sync, next, success);

                // RMW Atomicity. Future stores must happen after this, because we
                // already observed a value.
                self.last_successful_rmw.join(&threads.active().causality);

                Ok(prev)
            }
            Err(e) => {
                if is_seq_cst(failure) {
                    self.sc_load(index);
                }

                self.stores[index].sync.sync_load(threads, failure);
                Err(e)
            }
        }
    }

    fn apply_load_coherence(&mut self, threads: &mut thread::Set, index: usize) {
        for i in 0..self.stores.len() {
            // Skip if it is the current load.
            if index == i {
                continue;
            }

            // READ-READ coherence
            if self.stores[i].first_seen.is_seen_by_current(threads) {
                let mo = self.stores[i].modification_order;
                self.stores[index].modification_order.join(&mo);
            }

            // WRITE-READ coherence
            if self.stores[i].happens_before < threads.active().causality {
                let mo = self.stores[i].modification_order;
                self.stores[index].modification_order.join(&mo);
            }
        }
    }

    /// Track an atomic load
    fn track_load(&mut self, threads: &thread::Set) {
        assert!(!self.is_mutating, "atomic cell is in `with_mut` call");

        let current = &threads.active().causality;

        if let Some(mut_at) = current.ahead(&self.unsync_mut_at) {
            location::panic("Causality violation: Concurrent load and mut accesses.")
                .location("created", self.created_location)
                .thread("with_mut", mut_at, self.unsync_mut_locations[mut_at])
                .thread("load", threads.active_id(), self.loaded_locations[threads])
                .fire();
        }

        self.loaded_at.join(current);
    }

    /// Track an unsynchronized load
    fn track_unsync_load(&mut self, threads: &thread::Set) {
        assert!(!self.is_mutating, "atomic cell is in `with_mut` call");

        let current = &threads.active().causality;

        if let Some(mut_at) = current.ahead(&self.unsync_mut_at) {
            location::panic("Causality violation: Concurrent `unsync_load` and mut accesses.")
                .location("created", self.created_location)
                .thread("with_mut", mut_at, self.unsync_mut_locations[mut_at])
                .thread(
                    "unsync_load",
                    threads.active_id(),
                    self.unsync_loaded_locations[threads],
                )
                .fire();
        }

        if let Some(stored) = current.ahead(&self.stored_at) {
            location::panic("Causality violation: Concurrent `unsync_load` and atomic store.")
                .location("created", self.created_location)
                .thread("atomic store", stored, self.stored_locations[stored])
                .thread(
                    "unsync_load",
                    threads.active_id(),
                    self.unsync_loaded_locations[threads],
                )
                .fire();
        }

        self.unsync_loaded_at.join(current);
    }

    /// Track an atomic store
    fn track_store(&mut self, threads: &thread::Set) {
        assert!(!self.is_mutating, "atomic cell is in `with_mut` call");

        let current = &threads.active().causality;

        if let Some(mut_at) = current.ahead(&self.unsync_mut_at) {
            location::panic("Causality violation: Concurrent atomic store and mut accesses.")
                .location("created", self.created_location)
                .thread("with_mut", mut_at, self.unsync_mut_locations[mut_at])
                .thread(
                    "atomic store",
                    threads.active_id(),
                    self.stored_locations[threads],
                )
                .fire();
        }

        if let Some(loaded) = current.ahead(&self.unsync_loaded_at) {
            location::panic(
                "Causality violation: Concurrent atomic store and `unsync_load` accesses.",
            )
            .location("created", self.created_location)
            .thread("unsync_load", loaded, self.unsync_loaded_locations[loaded])
            .thread(
                "atomic store",
                threads.active_id(),
                self.stored_locations[threads],
            )
            .fire();
        }

        self.stored_at.join(current);
    }

    /// Track an unsynchronized mutation
    fn track_unsync_mut(&mut self, threads: &thread::Set) {
        assert!(!self.is_mutating, "atomic cell is in `with_mut` call");

        let current = &threads.active().causality;

        if let Some(loaded) = current.ahead(&self.loaded_at) {
            location::panic("Causality violation: Concurrent atomic load and unsync mut accesses.")
                .location("created", self.created_location)
                .thread("atomic load", loaded, self.loaded_locations[loaded])
                .thread(
                    "with_mut",
                    threads.active_id(),
                    self.unsync_mut_locations[threads],
                )
                .fire();
        }

        if let Some(loaded) = current.ahead(&self.unsync_loaded_at) {
            location::panic(
                "Causality violation: Concurrent `unsync_load` and unsync mut accesses.",
            )
            .location("created", self.created_location)
            .thread("unsync_load", loaded, self.unsync_loaded_locations[loaded])
            .thread(
                "with_mut",
                threads.active_id(),
                self.unsync_mut_locations[threads],
            )
            .fire();
        }

        if let Some(stored) = current.ahead(&self.stored_at) {
            location::panic(
                "Causality violation: Concurrent atomic store and unsync mut accesses.",
            )
            .location("created", self.created_location)
            .thread("atomic store", stored, self.stored_locations[stored])
            .thread(
                "with_mut",
                threads.active_id(),
                self.unsync_mut_locations[threads],
            )
            .fire();
        }

        if let Some(mut_at) = current.ahead(&self.unsync_mut_at) {
            location::panic("Causality violation: Concurrent unsync mut accesses.")
                .location("created", self.created_location)
                .thread("with_mut one", mut_at, self.unsync_mut_locations[mut_at])
                .thread(
                    "with_mut two",
                    threads.active_id(),
                    self.unsync_mut_locations[threads],
                )
                .fire();
        }

        self.unsync_mut_at.join(current);
    }

    /// Find all stores that could be returned by an atomic load.
    fn match_load_to_stores(
        &self,
        threads: &thread::Set,
        dst: &mut [u8],
        ordering: Ordering,
    ) -> usize {
        let mut n = 0;
        let cnt = self.cnt as usize;

        // We only need to consider loads as old as the **most** recent load
        // seen by each thread in the current causality.
        //
        // This probably isn't the smartest way to implement this, but someone
        // else can figure out how to improve on it if it turns out to be a
        // bottleneck.
        //
        // Add all stores **unless** a newer store has already been seen by the
        // current thread's causality.
        let before = self.mo_before();
        let thread = threads.active_id().as_usize();

        'outer: for (i, store_i) in self.stores.iter().take(cnt).enumerate() {
            if store_i.fence_stale & (1 << thread) != 0 {
                // The thread has executed a `SeqCst` fence since this store
                // fell behind the `SeqCst` order.
                continue 'outer;
            }

            if is_seq_cst(ordering) && self.is_sc_stale(i, &before) {
                // A `SeqCst` load cannot go back behind the `SeqCst` order.
                continue 'outer;
            }

            for (j, store_j) in self.stores.iter().take(cnt).enumerate() {
                if i == j {
                    continue;
                }

                let mo_i = store_i.modification_order;
                let mo_j = store_j.modification_order;

                // TODO: this sometimes fails
                assert_ne!(mo_i, mo_j);

                if before[j] & (1 << i) != 0 {
                    if store_j.first_seen.is_seen_by_current(threads) {
                        // Store `j` is newer, so don't store the current one.
                        continue 'outer;
                    }

                    if store_i.first_seen.is_seen_before_yield(threads) {
                        // Saw this load before the previous yield. In order to
                        // advance the model, don't return it again.
                        continue 'outer;
                    }

                    if is_seq_cst(ordering) && store_i.seq_cst && store_j.seq_cst {
                        // There is a newer SeqCst store
                        continue 'outer;
                    }
                }
            }

            // The load may return this store
            dst[n] = i as u8;
            n += 1;
        }

        n
    }

    fn match_rmw_to_stores(&self, dst: &mut [u8]) -> usize {
        let mut n = 0;
        let cnt = self.cnt as usize;

        // Unlike `match_load_to_stores`, rmw operations only load "newest"
        // stores, in terms of modification order.
        let before = self.mo_before();

        'outer: for (i, store_i) in self.stores.iter().take(cnt).enumerate() {
            for (j, store_j) in self.stores.iter().take(cnt).enumerate() {
                if i == j {
                    continue;
                }

                let mo_i = store_i.modification_order;
                let mo_j = store_j.modification_order;

                assert_ne!(mo_i, mo_j);

                if before[j] & (1 << i) != 0 {
                    // There is a newer store.
                    continue 'outer;
                }
            }

            // The load may return this store
            dst[n] = i as u8;
            n += 1;
        }

        n
    }

    fn stores_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Store> {
        let (start, end) = range(self.cnt);
        let (two, one) = self.stores[..end].split_at_mut(start);

        one.iter_mut().chain(two.iter_mut())
    }

    /// Returns the last dependent access
    /// Returns the previous accesses that an access of kind `action` is
    /// dependent with.
    pub(super) fn dependent_accesses(&self, action: Action) -> impl Iterator<Item = &Access> {
        // Everything is dependent with the last store / rmw. Only stores and
        // rmws are dependent with loads, except that `SeqCst` loads are
        // ordered among themselves.
        let (loads, sc_load) = match action {
            Action::Load => (&[][..], None),
            Action::LoadSeqCst => (&[][..], self.last_sc_load_access.as_ref()),
            _ => (&self.last_load_access[..], None),
        };

        self.last_non_load_access
            .iter()
            .chain(loads.iter().flatten())
            .chain(sc_load)
    }

    /// Records an access by `thread`.
    pub(super) fn set_last_access(
        &mut self,
        action: Action,
        thread: usize,
        path_id: usize,
        version: &VersionVec,
    ) {
        if action.is_load() {
            Access::set_or_create(&mut self.last_load_access[thread], path_id, version);

            if action.is_seq_cst() {
                Access::set_or_create(&mut self.last_sc_load_access, path_id, version);
            }
        } else {
            // Stores / RMWs. These are ordered after all earlier loads,
            // which therefore no longer need to be tracked.
            Access::set_or_create(&mut self.last_non_load_access, path_id, version);
            self.last_load_access = Default::default();
            self.last_sc_load_access = None;
        }
    }
}

/// The slots in a bit set of slots.
fn slots(set: u8) -> impl Iterator<Item = usize> {
    (0..MAX_ATOMIC_HISTORY).filter(move |slot| set & (1 << slot) != 0)
}

// ===== impl Store =====

impl Default for Store {
    fn default() -> Store {
        Store {
            value: 0,
            happens_before: VersionVec::new(),
            modification_order: VersionVec::new(),
            sync: Synchronize::new(),
            first_seen: FirstSeen::new(),
            seq_cst: false,
            mo_preds: 0,
            sc_stale: false,
            fence_stale: 0,
        }
    }
}

// ===== impl FirstSeen =====

impl FirstSeen {
    fn new() -> FirstSeen {
        FirstSeen([u16::MAX; MAX_THREADS])
    }

    fn touch(&mut self, threads: &thread::Set) {
        if self.0[threads.active_id().as_usize()] == u16::MAX {
            self.0[threads.active_id().as_usize()] = threads.active_atomic_version();
        }
    }

    fn is_seen_by_current(&self, threads: &thread::Set) -> bool {
        for (thread_id, version) in threads.active().causality.versions(threads.execution_id()) {
            match self.0[thread_id.as_usize()] {
                u16::MAX => {}
                v if v <= version => return true,
                _ => {}
            }
        }

        false
    }

    fn is_seen_before_yield(&self, threads: &thread::Set) -> bool {
        let thread_id = threads.active_id();

        let last_yield = match threads.active().last_yield {
            Some(v) => v,
            None => return false,
        };

        match self.0[thread_id.as_usize()] {
            u16::MAX => false,
            v => v <= last_yield,
        }
    }
}

fn is_seq_cst(order: Ordering) -> bool {
    order == Ordering::SeqCst
}

fn range(cnt: u16) -> (usize, usize) {
    let start = index(cnt.saturating_sub(MAX_ATOMIC_HISTORY as u16));
    let mut end = index(cmp::min(cnt, MAX_ATOMIC_HISTORY as u16));

    if end == 0 {
        end = MAX_ATOMIC_HISTORY;
    }

    assert!(
        start <= end,
        "[loom internal bug] cnt = {}; start = {}; end = {}",
        cnt,
        start,
        end
    );

    (start, end)
}

fn index(cnt: u16) -> usize {
    cnt as usize % MAX_ATOMIC_HISTORY
}
