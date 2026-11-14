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
use crate::rt::seq_cst::{Candidate, NodeSet, SeqCst, NO_LOCATION};
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

    /// Last time the atomic was accessed for a store or rmw operation. Every
    /// access before it is ordered before it, so nothing older needs to be
    /// remembered.
    last_non_load_access: Option<Access>,

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

    /// Stores (as a bit set of slots in `State::stores`) that are ordered
    /// before this one in the modification order, in addition to what
    /// `modification_order` says.
    ///
    /// A load that returns this store places it after the stores the loading
    /// thread already knew of. The store exists by then, with other stores
    /// possibly ordered after it, so that cannot be expressed by changing its
    /// version vector.
    mo_preds: u8,

    /// The `SeqCst` operations and fences that are related to this store,
    /// which is what the order of those operations depends on.
    sc: StoreNodes,

    /// For a store made by a read-modify-write: the slot of the store it
    /// read. Nothing may come between the two in the modification order.
    rmw_of: Option<u8>,
}

/// The nodes of the `SeqCst` order (see `rt::seq_cst`) that are related to a
/// store.
#[derive(Debug, Clone, Default)]
struct StoreNodes {
    /// The store itself, if it was made with `SeqCst` ordering.
    store: Option<usize>,

    /// Fences that happen before the store.
    fences_before: NodeSet,

    /// Fences that the store happens before.
    fences_after: NodeSet,

    /// `SeqCst` loads that returned the store.
    loads: NodeSet,

    /// Fences that happen before a load that returned the store.
    load_fences_before: NodeSet,

    /// Fences that a load which returned the store happens before.
    load_fences_after: NodeSet,
}

impl StoreNodes {
    /// What counts as this store when it is the earlier of two stores: the
    /// store or a load of it, and fences before those.
    fn sources(&self) -> NodeSet {
        let mut nodes = self.fences_before.clone();

        nodes.union_with(&self.loads);
        nodes.union_with(&self.load_fences_before);

        if let Some(store) = self.store {
            nodes.insert(store);
        }

        nodes
    }

    /// What counts as this store when it is the later of two stores: the
    /// store, and fences after it.
    fn targets(&self) -> NodeSet {
        let mut nodes = self.fences_after.clone();

        if let Some(store) = self.store {
            nodes.insert(store);
        }

        nodes
    }

    /// The edges of the `SeqCst` order that follow from `earlier` being
    /// ordered before `later` in the modification order.
    fn edges(earlier: &StoreNodes, later: &StoreNodes, edges: &mut Vec<(usize, usize)>) {
        product(&earlier.sources(), &later.targets(), edges);

        // Fences are also ordered through loads of the later store.
        let mut fences = earlier.fences_before.clone();
        fences.union_with(&earlier.load_fences_before);

        product(&fences, &later.load_fences_after, edges);
    }
}

fn product(from: &NodeSet, to: &NodeSet, edges: &mut Vec<(usize, usize)>) {
    if from.is_empty() || to.is_empty() {
        return;
    }

    for from in from.iter() {
        for to in to.iter() {
            // An operation is not ordered with respect to itself: a
            // read-modify-write is both a load of one store and the next
            // store.
            if from != to {
                edges.push((from, to));
            }
        }
    }
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

    rt::synchronize(|execution| {
        let thread = execution.threads.active_id().as_usize();
        let clock = execution.threads.active().causality.get(thread);

        // A fence is an operation on no location in particular.
        execution.seq_cst.observe(thread, NO_LOCATION, clock);

        match ordering {
            Ordering::Acquire => fence_acq(execution),
            Ordering::Release => fence_rel(execution),
            Ordering::AcqRel => fence_acqrel(execution),
            Ordering::SeqCst => fence_seqcst(execution),
            Ordering::Relaxed => panic!("there is no such thing as a relaxed fence"),
            order => unimplemented!("unimplemented ordering {:?}", order),
        }

        execution.threads.active_mut().note_access(NO_LOCATION);
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

    // The fence takes its place in the order of `SeqCst` operations.
    let thread = execution.threads.active_id().as_usize();
    let causality = execution.threads.active().causality;
    let elsewhere = execution.threads.active().elsewhere(NO_LOCATION);

    let node = execution.seq_cst.add(&Candidate {
        thread,
        clock: causality.get(thread),
        location: NO_LOCATION,
        causality: &causality,
        elsewhere: &elsewhere,
    });

    for state in execution.objects.iter_mut::<State>() {
        state.note_fence(&execution.threads, node);
        state.commit(&mut execution.seq_cst);
    }
}

impl<T: Numeric> Atomic<T> {
    /// Create a new, atomic cell initialized with the provided value
    pub(crate) fn new(value: T, location: Location) -> Atomic<T> {
        rt::execution(|execution| {
            let state = State::new(
                &mut execution.threads,
                &mut execution.seq_cst,
                execution.objects.next_index(),
                value.into_u64(),
                location,
            );
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
            let atomic = self.begin(execution);
            let state = self.state.get_mut(&mut execution.objects);

            // If necessary, generate the list of stores to permute through
            if execution.path.is_traversed() {
                let mut seed = [0; MAX_ATOMIC_HISTORY];

                let n = state.match_load_to_stores(
                    &execution.threads,
                    &execution.seq_cst,
                    atomic,
                    &mut seed[..],
                    ordering,
                );

                execution.path.push_load(&seed[..n]);
            }

            // Get the store to return from this load.
            let index = execution.path.branch_load();

            trace!(state = ?self.state, ?ordering, %location, "Atomic::load");

            let value = state.load(
                &mut execution.threads,
                &mut execution.seq_cst,
                atomic,
                index,
                location,
                ordering,
            );

            execution.threads.active_mut().note_access(atomic);

            T::from_u64(value)
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
            let atomic = self.begin(execution);
            let state = self.state.get_mut(&mut execution.objects);

            state.stored_locations.track(location, &execution.threads);

            // An atomic store counts as a read access to the underlying memory
            // cell.
            state.track_store(&execution.threads);

            trace!(state = ?self.state, ?ordering, %location, "Atomic::store");

            // Do the store
            state.store(
                &mut execution.threads,
                &mut execution.seq_cst,
                atomic,
                Synchronize::new(),
                val.into_u64(),
                ordering,
                None,
            );
            state.commit(&mut execution.seq_cst);

            execution.threads.active_mut().note_access(atomic);
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
            let atomic = self.begin(execution);
            let state = self.state.get_mut(&mut execution.objects);

            // If necessary, generate the list of stores to permute through
            if execution.path.is_traversed() {
                let mut seed = [0; MAX_ATOMIC_HISTORY];

                let n = state.match_rmw_to_stores(
                    &execution.threads,
                    &execution.seq_cst,
                    atomic,
                    &mut seed[..],
                    success,
                    failure,
                );
                execution.path.push_load(&seed[..n]);
            }

            // Get the store to use for the read portion of the rmw operation.
            let index = execution.path.branch_load();

            trace!(state = ?self.state, ?success, ?failure, %location, "Atomic::rmw");

            let result = state.rmw(
                &mut execution.threads,
                &mut execution.seq_cst,
                atomic,
                index,
                location,
                success,
                failure,
                |num| f(T::from_u64(num)).map(T::into_u64),
            );

            execution.threads.active_mut().note_access(atomic);

            result.map(T::from_u64)
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

    /// Start of an atomic operation by the current thread. Returns what
    /// identifies this atomic as a location.
    fn begin(&self, execution: &mut Execution) -> usize {
        let atomic = self.state.index();
        let thread = execution.threads.active_id().as_usize();
        let clock = execution.threads.active().causality.get(thread);

        execution.seq_cst.observe(thread, atomic, clock);

        atomic
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
    fn new(
        threads: &mut thread::Set,
        seq_cst: &mut SeqCst,
        atomic: usize,
        value: u64,
        location: Location,
    ) -> State {
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
            last_non_load_access: None,
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
        state.store(
            threads,
            seq_cst,
            atomic,
            Synchronize::new(),
            value,
            Ordering::Release,
            None,
        );
        state.commit(seq_cst);

        state
    }

    fn load(
        &mut self,
        threads: &mut thread::Set,
        seq_cst: &mut SeqCst,
        atomic: usize,
        index: usize,
        location: Location,
        ordering: Ordering,
    ) -> u64 {
        self.loaded_locations.track(location, threads);
        // Validate memory safety
        self.track_load(threads);

        // Apply coherence rules
        self.apply_load_coherence(threads, index);

        let store = &mut self.stores[index];

        store.first_seen.touch(threads);
        store.sync.sync_load(threads, ordering);

        let value = store.value;

        self.note_load(threads, seq_cst, atomic, index, is_seq_cst(ordering));
        self.commit(seq_cst);

        value
    }

    /// Adds a store. `node` is the store's place in the order of `SeqCst`
    /// operations if it already has one, which is the case for the store
    /// half of a read-modify-write.
    #[allow(clippy::too_many_arguments)]
    fn store(
        &mut self,
        threads: &mut thread::Set,
        seq_cst: &mut SeqCst,
        atomic: usize,
        mut sync: Synchronize,
        value: u64,
        ordering: Ordering,
        node: Option<usize>,
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

        // The slot is about to be reused; forget what was known about the
        // store it held.
        self.forget_slot(index);

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
            mo_preds: 0,
            sc: StoreNodes::default(),
            rmw_of: None,
        };

        self.note_store(threads, seq_cst, atomic, index, is_seq_cst(ordering), node);
    }

    /// The modification order as far as it is known: bit `i` of entry `j` is
    /// set if the store in slot `i` is ordered before the one in slot `j`.
    fn mo_before(&self) -> [u8; MAX_ATOMIC_HISTORY] {
        self.mo_before_with(0, 0)
    }

    /// The modification order as it would be if the stores in the slots of
    /// `preds` were ordered before the store in `slot` as well.
    fn mo_before_with(&self, slot: usize, preds: u8) -> [u8; MAX_ATOMIC_HISTORY] {
        let cnt = cmp::min(self.cnt as usize, MAX_ATOMIC_HISTORY);
        let mut before = [0u8; MAX_ATOMIC_HISTORY];

        for j in 0..cnt {
            before[j] = self.stores[j].mo_preds & !(1 << j);

            if j == slot {
                before[j] |= preds & !(1 << j);
            }

            for i in 0..cnt {
                if i != j && self.stores[i].modification_order < self.stores[j].modification_order {
                    before[j] |= 1 << i;
                }
            }
        }

        loop {
            // Transitive closure
            for k in 0..cnt {
                for j in 0..cnt {
                    if before[j] & (1 << k) != 0 {
                        before[j] |= before[k];
                    }
                }
            }

            // RMW atomicity: the store written by a read-modify-write comes
            // right after the store it read. Whatever is ordered after the
            // store that was read is therefore ordered after the one that
            // was written, too. That includes stores that existed, with no
            // known position, when the read-modify-write happened, and got
            // ordered after the store it read only later.
            let mut changed = false;

            for written in 0..cnt {
                let read = match self.stores[written].rmw_of {
                    Some(read) => read as usize,
                    None => continue,
                };

                for other in 0..cnt {
                    if other == written || other == read {
                        continue;
                    }

                    let after_read = before[other] & (1 << read) != 0;
                    let after_written = before[other] & (1 << written) != 0;
                    let before_written = before[written] & (1 << other) != 0;

                    if after_read && !after_written && !before_written {
                        before[other] |= 1 << written;
                        changed = true;
                    }

                    // And the other way around: whatever is ordered before
                    // the store that was written is ordered before the one
                    // that was read.
                    let before_read = before[read] & (1 << other) != 0;

                    if before_written && !before_read && !after_read {
                        before[read] |= 1 << other;
                        changed = true;
                    }
                }
            }

            if !changed {
                break;
            }
        }

        before
    }

    /// The stores the current thread already knows of, which are ordered
    /// before the store in slot `index` once the thread loads that.
    fn coherence_preds(
        &self,
        threads: &thread::Set,
        index: usize,
        before: &[u8; MAX_ATOMIC_HISTORY],
    ) -> u8 {
        let cnt = cmp::min(self.cnt as usize, MAX_ATOMIC_HISTORY);
        let mut preds = 0;

        for i in 0..cnt {
            // Skip if it is the current load.
            if index == i {
                continue;
            }

            // READ-READ coherence
            let seen = self.stores[i].first_seen.is_seen_by_current(threads);

            // WRITE-READ coherence
            let happens_before = self.stores[i].happens_before < threads.active().causality;

            if (seen || happens_before) && before[i] & (1 << index) == 0 {
                preds |= 1 << i;
            }
        }

        preds
    }

    /// The operation the current thread is executing on this atomic, as a
    /// candidate for the order of `SeqCst` operations. `causality` is what
    /// the thread knows of once the operation has executed.
    fn candidate<'a>(
        threads: &thread::Set,
        atomic: usize,
        causality: &'a VersionVec,
        elsewhere: &'a VersionVec,
    ) -> Candidate<'a> {
        let thread = threads.active_id().as_usize();

        Candidate {
            thread,
            clock: threads.active().causality.get(thread),
            location: atomic,
            causality,
            elsewhere,
        }
    }

    /// The current thread has loaded the store in slot `index`. Returns the
    /// load's place in the order of `SeqCst` operations, if it has one.
    fn note_load(
        &mut self,
        threads: &thread::Set,
        seq_cst: &mut SeqCst,
        atomic: usize,
        index: usize,
        is_seq_cst: bool,
    ) -> Option<usize> {
        if seq_cst.is_empty() && !is_seq_cst {
            // The program has not used `SeqCst` so far.
            return None;
        }

        let thread = threads.active_id().as_usize();
        let causality = threads.active().causality;

        let fences = seq_cst.fences_before(thread, &causality);
        self.stores[index].sc.load_fences_before.union_with(&fences);

        if !is_seq_cst {
            return None;
        }

        let elsewhere = threads.active().elsewhere(atomic);
        let node = seq_cst.add(&Self::candidate(threads, atomic, &causality, &elsewhere));

        self.stores[index].sc.loads.insert(node);

        Some(node)
    }

    /// The current thread has made the store in slot `index`.
    fn note_store(
        &mut self,
        threads: &thread::Set,
        seq_cst: &mut SeqCst,
        atomic: usize,
        index: usize,
        is_seq_cst: bool,
        node: Option<usize>,
    ) {
        if seq_cst.is_empty() && !is_seq_cst {
            return;
        }

        let thread = threads.active_id().as_usize();
        let causality = threads.active().causality;

        self.stores[index].sc.fences_before = seq_cst.fences_before(thread, &causality);

        self.stores[index].sc.store = match node {
            Some(node) => Some(node),
            None if is_seq_cst => {
                let elsewhere = threads.active().elsewhere(atomic);
                Some(seq_cst.add(&Self::candidate(threads, atomic, &causality, &elsewhere)))
            }
            None => None,
        };
    }

    /// The current thread has executed a `SeqCst` fence.
    fn note_fence(&mut self, threads: &thread::Set, node: usize) {
        let cnt = cmp::min(self.cnt as usize, MAX_ATOMIC_HISTORY);

        for store in &mut self.stores[..cnt] {
            if store.happens_before <= threads.active().causality {
                store.sc.fences_after.insert(node);
            }

            if store.first_seen.is_seen_by_current(threads) {
                store.sc.load_fences_after.insert(node);
            }
        }
    }

    /// The edges of the order of `SeqCst` operations that follow from the
    /// modification order `before` and the given per-store nodes.
    fn seq_cst_edges(
        before: &[u8; MAX_ATOMIC_HISTORY],
        nodes: &[StoreNodes],
    ) -> Vec<(usize, usize)> {
        let mut edges = Vec::new();

        for (later, later_nodes) in nodes.iter().enumerate() {
            for earlier in slots(before[later]) {
                if earlier < nodes.len() {
                    StoreNodes::edges(&nodes[earlier], later_nodes, &mut edges);
                }
            }

            // A fence before a store is ordered before a fence after a load
            // of that store.
            product(
                &later_nodes.fences_before,
                &later_nodes.load_fences_after,
                &mut edges,
            );
        }

        edges
    }

    fn store_nodes(&self) -> Vec<StoreNodes> {
        let cnt = cmp::min(self.cnt as usize, MAX_ATOMIC_HISTORY);

        self.stores[..cnt]
            .iter()
            .map(|store| store.sc.clone())
            .collect()
    }

    /// Makes the order of `SeqCst` operations reflect the current state of
    /// this atomic.
    fn commit(&mut self, seq_cst: &mut SeqCst) {
        if seq_cst.is_empty() {
            return;
        }

        let before = self.mo_before();

        for (from, to) in Self::seq_cst_edges(&before, &self.store_nodes()) {
            let added = seq_cst.add_edge(from, to);

            // Loads only return stores for which this works out, and nothing
            // else settles the order of two existing stores.
            debug_assert!(added, "[loom internal bug] SeqCst order is cyclic");
        }
    }

    /// The stores (as a bit set of slots) that are ordered after the one in
    /// slot `index`.
    fn later_stores(&self, index: usize) -> u8 {
        let cnt = cmp::min(self.cnt as usize, MAX_ATOMIC_HISTORY);
        let before = self.mo_before();

        (0..cnt)
            .filter(|&slot| slot != index && before[slot] & (1 << index) != 0)
            .fold(0, |later, slot| later | 1 << slot)
    }

    /// Returns `true` if the current thread reading the store in slot `index`
    /// with a read-modify-write, and writing a new store directly after it,
    /// is consistent with the order of `SeqCst` operations.
    fn rmw_admissible(
        &self,
        threads: &thread::Set,
        seq_cst: &SeqCst,
        atomic: usize,
        index: usize,
        success: Ordering,
        failure: Ordering,
    ) -> bool {
        if seq_cst.is_empty() {
            return true;
        }

        // Whether the operation succeeds is not known yet. Assume that it
        // does, and that it is a `SeqCst` operation if it can be: that is the
        // case with the most to be consistent with.
        let is_seq_cst = is_seq_cst(success) || is_seq_cst(failure);

        let mut causality = threads.active().causality;

        if matches!(
            success,
            Ordering::Acquire | Ordering::AcqRel | Ordering::SeqCst
        ) {
            causality.join(self.stores[index].sync.happens_before());
        }

        let thread = threads.active_id().as_usize();
        let elsewhere = threads.active().elsewhere(atomic);
        let candidate = Self::candidate(threads, atomic, &causality, &elsewhere);
        let fences = seq_cst.fences_before(thread, &causality);

        // The load ...
        let mut nodes = self.store_nodes();

        nodes[index].load_fences_before.union_with(&fences);

        if is_seq_cst {
            nodes[index].loads.insert(seq_cst.len());
        }

        let preds = self.coherence_preds(threads, index, &self.mo_before());
        let before = self.mo_before_with(index, preds);

        let mut edges = Self::seq_cst_edges(&before, &nodes);

        // ... and the store, which goes directly after the store that is
        // read. Its slot may be that of the oldest store, which is forgotten
        // at that point.
        let written = StoreNodes {
            store: if is_seq_cst {
                Some(seq_cst.len())
            } else {
                None
            },
            fences_before: fences,
            ..StoreNodes::default()
        };

        let cnt = cmp::min(self.cnt as usize, MAX_ATOMIC_HISTORY);
        let reused = if self.cnt as usize >= MAX_ATOMIC_HISTORY {
            Some(self.cnt as usize % MAX_ATOMIC_HISTORY)
        } else {
            None
        };

        for slot in (0..cnt).filter(|&slot| Some(slot) != reused) {
            if slot == index || before[index] & (1 << slot) != 0 {
                StoreNodes::edges(&nodes[slot], &written, &mut edges);
            } else if before[slot] & (1 << index) != 0 {
                StoreNodes::edges(&written, &nodes[slot], &mut edges);
            }
        }

        seq_cst.admits(if is_seq_cst { Some(&candidate) } else { None }, &edges)
    }

    /// Returns `true` if the current thread loading the store in slot `index`
    /// is consistent with the order of `SeqCst` operations.
    fn load_admissible(
        &self,
        threads: &thread::Set,
        seq_cst: &SeqCst,
        atomic: usize,
        index: usize,
        ordering: Ordering,
        is_seq_cst: bool,
    ) -> bool {
        if seq_cst.is_empty() {
            // Nothing to be consistent with. (A first `SeqCst` load only gets
            // edges that lead to it.)
            return true;
        }

        // What the state would be after the load ...
        let mut causality = threads.active().causality;

        if matches!(
            ordering,
            Ordering::Acquire | Ordering::AcqRel | Ordering::SeqCst
        ) {
            causality.join(self.stores[index].sync.happens_before());
        }

        let thread = threads.active_id().as_usize();
        let elsewhere = threads.active().elsewhere(atomic);
        let candidate = Self::candidate(threads, atomic, &causality, &elsewhere);

        let mut nodes = self.store_nodes();

        nodes[index]
            .load_fences_before
            .union_with(&seq_cst.fences_before(thread, &causality));

        if is_seq_cst {
            nodes[index].loads.insert(seq_cst.len());
        }

        let preds = self.coherence_preds(threads, index, &self.mo_before());
        let before = self.mo_before_with(index, preds);

        // ... and whether the order of `SeqCst` operations can accommodate
        // that.
        let edges = Self::seq_cst_edges(&before, &nodes);

        seq_cst.admits(if is_seq_cst { Some(&candidate) } else { None }, &edges)
    }

    fn forget_slot(&mut self, slot: usize) {
        let mask = !(1u8 << slot);

        for store in &mut self.stores {
            store.mo_preds &= mask;

            if store.rmw_of == Some(slot as u8) {
                store.rmw_of = None;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rmw<E>(
        &mut self,
        threads: &mut thread::Set,
        seq_cst: &mut SeqCst,
        atomic: usize,
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

        // The stores that are ordered after the one being read. The thread
        // knows of none of them, or it could not read this one.
        let later = self.later_stores(index);

        match f(prev) {
            Ok(next) => {
                // Perform load synchronization using the `success` ordering.
                //
                // This has to come before the store is validated: the store
                // half of the RMW happens after everything the load half
                // synchronizes with. Checking it against the thread's
                // causality from before the load reports accesses that the
                // load orders before this store as concurrent with it.
                self.stores[index].sync.sync_load(threads, success);

                // A `SeqCst` read-modify-write is a single operation in the
                // order of `SeqCst` operations: a load of the store it read
                // and the store it wrote.
                let node = self.note_load(threads, seq_cst, atomic, index, is_seq_cst(success));

                self.stored_locations.track(location, threads);
                // Track a store operation happened
                self.track_store(threads);

                // Store the new value, initializing with the `sync` value from
                // the load. This is our (hacky) way to establish a release
                // sequence.
                let sync = self.stores[index].sync;
                self.store(threads, seq_cst, atomic, sync, next, success, node);

                // Remember which store this one replaced, unless its slot was
                // just reused for the new store.
                let written = (self.cnt as usize - 1) % MAX_ATOMIC_HISTORY;

                if written != index {
                    self.stores[written].rmw_of = Some(index as u8);
                }

                // RMW atomicity: the new store goes directly after the one
                // that was read, and so before everything that was already
                // ordered after that one. (Stores that get ordered after the
                // one that was read later on are taken care of by
                // `mo_before`.)
                for slot in slots(later) {
                    if slot != written {
                        self.stores[slot].mo_preds |= 1 << written;
                    }
                }

                self.commit(seq_cst);

                Ok(prev)
            }
            Err(e) => {
                self.stores[index].sync.sync_load(threads, failure);

                self.note_load(threads, seq_cst, atomic, index, is_seq_cst(failure));
                self.commit(seq_cst);

                Err(e)
            }
        }
    }

    /// The current thread loads the store in slot `index`: every store the
    /// thread already knows about comes before it in the modification order.
    ///
    /// This is recorded as explicit edges, and not by joining the other
    /// stores' positions into this store's version vector. The store exists
    /// already and may have stores ordered after it; moving its vector moves
    /// it away from those, losing the fact that they come later. A thread
    /// could then load the store, and after it one that is known to be newer,
    /// only to be handed the older one again at the end.
    fn apply_load_coherence(&mut self, threads: &mut thread::Set, index: usize) {
        let before = self.mo_before();

        self.stores[index].mo_preds |= self.coherence_preds(threads, index, &before);
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
        seq_cst: &SeqCst,
        atomic: usize,
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

        'outer: for (i, store_i) in self.stores.iter().take(cnt).enumerate() {
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
                }
            }

            if !self.load_admissible(threads, seq_cst, atomic, i, ordering, is_seq_cst(ordering)) {
                // Returning this store contradicts the order of `SeqCst`
                // operations.
                continue 'outer;
            }

            // The load may return this store
            dst[n] = i as u8;
            n += 1;
        }

        assert!(
            n > 0,
            "[loom internal bug] no store an atomic load could return"
        );

        n
    }

    fn match_rmw_to_stores(
        &self,
        threads: &thread::Set,
        seq_cst: &SeqCst,
        atomic: usize,
        dst: &mut [u8],
        success: Ordering,
        failure: Ordering,
    ) -> usize {
        let mut n = 0;
        let cnt = self.cnt as usize;

        // A read-modify-write writes directly after the store it reads. Like
        // a load, it cannot read a store if the thread knows of a newer one:
        // what it writes comes after everything the thread knows of. It can
        // read a store that has newer stores the thread does not know of,
        // which then come after what it writes.
        let before = self.mo_before();

        'outer: for (i, store_i) in self.stores.iter().take(cnt).enumerate() {
            for (j, store_j) in self.stores.iter().take(cnt).enumerate() {
                if i == j {
                    continue;
                }

                let mo_i = store_i.modification_order;
                let mo_j = store_j.modification_order;

                assert_ne!(mo_i, mo_j);

                if store_j.rmw_of == Some(i as u8) {
                    // Another read-modify-write has read this store, and
                    // wrote directly after it.
                    continue 'outer;
                }

                if before[j] & (1 << i) != 0 {
                    if store_j.first_seen.is_seen_by_current(threads) {
                        // Store `j` is newer, and known to the thread.
                        continue 'outer;
                    }

                    if store_i.first_seen.is_seen_before_yield(threads) {
                        // Saw this store before the previous yield. In order
                        // to advance the model, don't return it again.
                        continue 'outer;
                    }
                }
            }

            if !self.rmw_admissible(threads, seq_cst, atomic, i, success, failure) {
                continue 'outer;
            }

            // The load may return this store
            dst[n] = i as u8;
            n += 1;
        }

        assert!(
            n > 0,
            "[loom internal bug] no store an atomic read-modify-write could read"
        );

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
        // rmws are dependent with loads.
        //
        // That goes for `SeqCst` loads as well. Which stores two of them can
        // return is restricted by the order of `SeqCst` operations, but that
        // order is not the order in which loom executes them, so executing
        // them the other way around allows nothing new.
        let loads = if action.is_load() {
            &[][..]
        } else {
            &self.last_load_access[..]
        };

        self.last_non_load_access
            .iter()
            .chain(loads.iter().flatten())
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
        } else {
            // Stores / RMWs. These are ordered after all earlier loads,
            // which therefore no longer need to be tracked.
            Access::set_or_create(&mut self.last_non_load_access, path_id, version);
            self.last_load_access = Default::default();
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
            mo_preds: 0,
            sc: StoreNodes::default(),
            rmw_of: None,
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
