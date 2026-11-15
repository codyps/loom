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

    /// The threads (as a bit set) for which a weak compare-exchange on this
    /// atomic has failed spuriously.
    spuriously_failed: crate::rt::ThreadSet,

    /// Currently tracked stored values. This is the `MAX_ATOMIC_HISTORY` most
    /// recent stores to the atomic cell in loom execution order.
    stores: Vec<Store>,

    /// How many of the slots are in use: the number of stores that are
    /// kept, and that a load can still return. At most
    /// `MAX_ATOMIC_HISTORY`.
    history: usize,

    /// The slot of the store that was made most recently.
    last: usize,

    /// What is left of the stores that are not kept anymore.
    forgotten: Forgotten,

    /// The slot of the store that a read-modify-write has read, while it
    /// makes its own.
    read_by_rmw: Option<usize>,

    /// What `mo_before` returned, if nothing that it is computed from has
    /// changed since. It is asked for several times per operation.
    mo_cache: MoCache,

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

    pub(super) fn is_load(self) -> bool {
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
    mo_preds: StoreSet,

    /// The `SeqCst` operations and fences that are related to this store,
    /// which is what the order of those operations depends on.
    sc: StoreNodes,

    /// For a store made by a read-modify-write: the slot of the store it
    /// read. Nothing may come between the two in the modification order.
    rmw_of: Option<u8>,

    /// The number of this store among the stores to the atomic, in the
    /// order in which they were made.
    seq: u16,

    /// `happens_before` of the stores that this one stands for, see
    /// `State::forget_slot`.
    stands_for: Vec<VersionVec>,
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

    /// What counted as the stores that had been forgotten when this one was
    /// made (see `State::forget_slot`): those come before it.
    forgotten: Forgotten,

    /// What counts as the stores that this one stands for when they are
    /// the later of two stores: the stores, and fences after them.
    stood_for: NodeSet,
}

/// The nodes of the `SeqCst` order that are related to stores that are not
/// kept anymore.
#[derive(Debug, Clone, Default)]
struct Forgotten {
    /// What counted as the stores when they were the earlier of two.
    sources: NodeSet,

    /// The fences among those.
    fences: NodeSet,
}

impl Forgotten {
    fn add(&mut self, store: &StoreNodes) {
        self.sources.union_with(&store.sources());
        self.fences.union_with(&store.fences_before);
        self.fences.union_with(&store.load_fences_before);
    }

    /// Drops the nodes that say nothing the others do not: what has to come
    /// after a node has to come after everything before that node. Without
    /// this the set would grow with every store that is forgotten.
    fn prune(&mut self, seq_cst: &SeqCst) {
        let sources: Vec<usize> = self.sources.iter().collect();

        self.sources.retain(|node| {
            !sources
                .iter()
                .any(|&other| other != node && seq_cst.precedes(node, other))
        });
    }

    /// The edges that follow from the forgotten stores being ordered before
    /// `later`.
    fn edges(&self, later: &StoreNodes, edges: &mut Vec<(usize, usize)>) {
        product(&self.sources, &later.own_targets(), edges);
        product(&self.fences, &later.load_fences_after, edges);
    }
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
        let mut nodes = self.own_targets();

        nodes.union_with(&self.stood_for);

        nodes
    }

    /// `targets`, without what belongs to the stores this one stands for.
    fn own_targets(&self) -> NodeSet {
        let mut nodes = self.fences_after.clone();

        if let Some(store) = self.store {
            nodes.insert(store);
        }

        nodes
    }

    /// Makes this stand for `earlier` as well, a store that this one comes
    /// directly after.
    fn absorb(&mut self, earlier: &StoreNodes) {
        self.fences_before.union_with(&earlier.fences_before);
        self.stood_for.union_with(&earlier.targets());
        self.loads.union_with(&earlier.loads);
        self.load_fences_before
            .union_with(&earlier.load_fences_before);
        self.load_fences_after
            .union_with(&earlier.load_fences_after);
        self.forgotten
            .sources
            .union_with(&earlier.forgotten.sources);
        self.forgotten.fences.union_with(&earlier.forgotten.fences);

        // What counted as the earlier store comes before what counts as
        // this one, including the fences that are yet to come after it.
        self.forgotten.add(earlier);

        // The store itself, where it is the earlier of two stores. (Where
        // it is the later one, it is among its targets, above.)
        if let Some(store) = earlier.store {
            self.loads.insert(store);
        }
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

/// A set of the stores of an atomic, by slot: one bit each.
type StoreSet = u16;

const _: () = assert!(MAX_ATOMIC_HISTORY <= StoreSet::BITS as usize);

/// A modification order: for each slot, the stores that come before the
/// one in it.
type MoOrder = [StoreSet; MAX_ATOMIC_HISTORY];

/// A modification order as `State::mo_before` returns it, or nothing.
///
/// The state of an atomic is shared between threads as far as the compiler
/// is concerned, so this is made of atomics.
#[derive(Debug, Default)]
struct MoCache {
    valid: std::sync::atomic::AtomicBool,
    before: [std::sync::atomic::AtomicU16; MAX_ATOMIC_HISTORY],
}

impl MoCache {
    fn get(&self) -> Option<MoOrder> {
        use std::sync::atomic::Ordering::Relaxed;

        if !self.valid.load(Relaxed) {
            return None;
        }

        let mut before = [0; MAX_ATOMIC_HISTORY];

        for (before, cached) in before.iter_mut().zip(&self.before) {
            *before = cached.load(Relaxed);
        }

        Some(before)
    }

    fn set(&self, before: MoOrder) {
        use std::sync::atomic::Ordering::Relaxed;

        for (before, cached) in before.iter().zip(&self.before) {
            cached.store(*before, Relaxed);
        }

        self.valid.store(true, Relaxed);
    }

    fn clear(&self) {
        self.valid
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
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
        execution.threads.active_mut().settle_other();
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
                execution.max_history,
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
            let index = state.last;
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

    /// Performs a read-modify-write: `f` is given the current value, and
    /// returns the value to store or the error to fail with.
    ///
    /// If `spurious` is given, the operation may also fail although `f`
    /// would have it succeed, with the error `spurious` returns for the
    /// current value. That is what a "weak" compare-exchange can do.
    pub(crate) fn rmw<F, E>(
        &self,
        location: Location,
        success: Ordering,
        failure: Ordering,
        f: F,
        spurious: Option<fn(T) -> E>,
    ) -> Result<T, E>
    where
        F: Fn(T) -> Result<T, E>,
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
                    |num| f(T::from_u64(num)).is_ok(),
                );
                execution.path.push_load(&seed[..n]);
            }

            // Get the store to use for the read portion of the rmw operation.
            let index = execution.path.branch_load();

            trace!(state = ?self.state, ?success, ?failure, %location, "Atomic::rmw");

            // Whether to fail spuriously is a choice loom explores both ways.
            // It is only made if the operation would succeed otherwise.
            let path = &mut execution.path;
            let spurious = spurious.map(|error| {
                (
                    move || path.branch_spurious(),
                    move |num| error(T::from_u64(num)),
                )
            });

            let result = state.rmw(
                &mut execution.threads,
                &mut execution.seq_cst,
                atomic,
                index,
                location,
                success,
                failure,
                |num| f(T::from_u64(num)).map(T::into_u64),
                spurious,
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
            let index = state.last;
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
                    let index = state.last;
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

        execution.threads.active_mut().settle_other();
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
        history: usize,
        value: u64,
        location: Location,
    ) -> State {
        assert!(
            (1..=MAX_ATOMIC_HISTORY).contains(&history),
            "the number of stores kept per atomic has to be between 1 and {}",
            MAX_ATOMIC_HISTORY
        );

        let mut state = State {
            history,
            last: 0,
            forgotten: Forgotten::default(),
            read_by_rmw: None,
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
            spuriously_failed: 0,
            stores: (0..history).map(|_| Store::default()).collect(),
            mo_cache: MoCache::default(),
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
        let index = if (self.cnt as usize) < self.history {
            self.cnt as usize
        } else {
            self.next_slot(&self.mo_before(), seq_cst, self.read_by_rmw)
        };

        self.mo_cache.clear();
        self.last = index;

        // Increment the count
        self.cnt += 1;

        // The modification order is initialized to the thread's current
        // causality. All reads / writes that happen before this store are
        // ordered before the store.
        let happens_before = threads.active().causality;

        // Starting with the thread's causality covers WRITE-WRITE coherence
        let mut modification_order = happens_before;

        // Apply coherence rules
        for i in 0..self.history {
            // READ-WRITE coherence
            if self.stores[i].first_seen.is_seen_by_current(threads) {
                let mo = self.stores[i].modification_order;
                modification_order.join(&mo);
            }
        }

        // The slot is about to be reused; forget what was known about the
        // store it held.
        self.forget_slot(seq_cst, index, self.cnt as usize > self.history);

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
            sc: StoreNodes {
                forgotten: self.forgotten.clone(),
                ..StoreNodes::default()
            },
            rmw_of: None,
            seq: self.cnt,
            stands_for: Vec::new(),
        };

        self.note_store(threads, seq_cst, atomic, index, is_seq_cst(ordering), node);
    }

    /// The modification order as far as it is known: bit `i` of entry `j` is
    /// set if the store in slot `i` is ordered before the one in slot `j`.
    fn mo_before(&self) -> MoOrder {
        if let Some(before) = self.mo_cache.get() {
            debug_assert_eq!(
                before,
                self.mo_before_with(0, 0),
                "[loom internal bug] stale modification order"
            );

            return before;
        }

        let before = self.mo_before_with(0, 0);
        self.mo_cache.set(before);

        before
    }

    /// The modification order as it would be if the stores in the slots of
    /// `preds` were ordered before the store in `slot` as well.
    fn mo_before_with(&self, slot: usize, preds: StoreSet) -> MoOrder {
        let cnt = cmp::min(self.cnt as usize, self.history);
        let mut before: MoOrder = [0; MAX_ATOMIC_HISTORY];

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
    fn coherence_preds(&self, threads: &thread::Set, index: usize, before: &MoOrder) -> StoreSet {
        let cnt = cmp::min(self.cnt as usize, self.history);
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
        let cnt = cmp::min(self.cnt as usize, self.history);

        for store in &mut self.stores[..cnt] {
            let causality = &threads.active().causality;

            // The fence comes after the store, or after one that the store
            // stands for.
            if store.happens_before <= *causality {
                store.sc.fences_after.insert(node);
            } else if store.stands_for.iter().any(|store| store <= causality) {
                store.sc.stood_for.insert(node);
            }

            if store.first_seen.is_seen_by_current(threads) {
                store.sc.load_fences_after.insert(node);
            }
        }
    }

    /// The edges of the order of `SeqCst` operations that follow from the
    /// modification order `before` and the given per-store nodes.
    fn seq_cst_edges(before: &MoOrder, nodes: &[StoreNodes]) -> Vec<(usize, usize)> {
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

            later_nodes.forgotten.edges(later_nodes, &mut edges);
        }

        edges
    }

    fn store_nodes(&self) -> Vec<StoreNodes> {
        let cnt = cmp::min(self.cnt as usize, self.history);

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
    fn later_stores(&self, index: usize) -> StoreSet {
        let cnt = cmp::min(self.cnt as usize, self.history);
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
        mo_before: &MoOrder,
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

        let preds = self.coherence_preds(threads, index, mo_before);
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

        let cnt = cmp::min(self.cnt as usize, self.history);
        let reused = if self.cnt as usize >= self.history {
            Some(self.next_slot(&before, seq_cst, Some(index)))
        } else {
            None
        };

        // The new store comes after the forgotten ones, the one that makes
        // room for it included.
        let mut written = written;
        written.forgotten = self.forgotten.clone();

        if let Some(reused) = reused {
            written.forgotten.add(&nodes[reused]);
        }

        written.forgotten.edges(&written, &mut edges);

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
        mo_before: &MoOrder,
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

        let preds = self.coherence_preds(threads, index, mo_before);
        let before = self.mo_before_with(index, preds);

        // ... and whether the order of `SeqCst` operations can accommodate
        // that.
        let edges = Self::seq_cst_edges(&before, &nodes);

        seq_cst.admits(if is_seq_cst { Some(&candidate) } else { None }, &edges)
    }

    /// Forgets the store in `slot`, which is the first of the stores that
    /// are kept (see `next_slot`), or not a store at all.
    fn forget_slot(&mut self, seq_cst: &mut SeqCst, slot: usize, in_use: bool) {
        let mask: StoreSet = !(1 << slot);

        if in_use {
            // What is known about the order of the other stores by way of
            // this one has to be known without it.
            let before = self.mo_before();
            let cnt = cmp::min(self.cnt as usize, self.history);

            for (other, before) in before.iter().enumerate().take(cnt) {
                self.stores[other].mo_preds |= *before;
            }
        }

        self.mo_cache.clear();

        // A store written by a read-modify-write that read this one comes
        // directly after it: whatever is before this store is before that
        // one, and whatever is, or turns out to be, after that one is after
        // this one. That store stands for both from here on: it takes this
        // one's place in the order, counts as seen by the threads that have
        // seen this one, and as this one in the order of `SeqCst`
        // operations.
        let follower = (0..self.history)
            .find(|&other| other != slot && self.stores[other].rmw_of == Some(slot as u8));

        if let Some(follower) = follower {
            let position = self.stores[slot].position();
            self.stores[follower].take_position(position);
        } else if self.read_by_rmw == Some(slot) {
            // The store that is being made is the one that follows it, see
            // `rmw`.
        } else if in_use && !seq_cst.is_empty() {
            // Nothing stands for the store anymore. It is taken to come
            // before every other one from here on. What that means for the
            // order of `SeqCst` operations is settled now for the stores
            // that are kept, and remembered for the ones to come.
            let mut edges = Vec::new();

            for other in (0..self.history).filter(|&other| other != slot) {
                StoreNodes::edges(&self.stores[slot].sc, &self.stores[other].sc, &mut edges);
            }

            for (from, to) in edges {
                // A store for which this does not work out was in fact the
                // earlier one. Nothing is going to say otherwise anymore.
                let _ = seq_cst.add_edge(from, to);
            }

            let forgotten = self.stores[slot].sc.clone();
            self.forgotten.add(&forgotten);
            self.forgotten.prune(seq_cst);

            // The stores that are kept come after it as well, and so does
            // what is yet to count as them: a fence after one of them, say.
            for other in (0..self.history).filter(|&other| other != slot) {
                self.stores[other].sc.forgotten.add(&forgotten);
            }
        }

        for store in &mut self.stores[..self.history] {
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
        spurious: Option<(impl FnOnce() -> bool, impl FnOnce(u64) -> E)>,
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

        let result = match (f(prev), spurious) {
            (Ok(next), Some((fail, error))) => {
                // A weak compare-exchange may fail although the value is the
                // expected one. Each thread gets to do that once per atomic:
                // a loop that retries until it succeeds then still ends, and
                // the model stays finite.
                let thread = 1 << threads.active_id().as_usize();

                if self.spuriously_failed & thread == 0 && fail() {
                    self.spuriously_failed |= thread;
                    Err(error(prev))
                } else {
                    Ok(next)
                }
            }
            (result, _) => result,
        };

        match result {
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
                let position = self.stores[index].position();

                self.read_by_rmw = Some(index);
                self.store(threads, seq_cst, atomic, sync, next, success, node);
                self.read_by_rmw = None;

                // Remember which store this one replaced. If its slot was
                // just reused for the new store, the new one takes its
                // place, see `forget_slot`.
                let written = self.last;

                self.mo_cache.clear();

                if written != index {
                    self.stores[written].rmw_of = Some(index as u8);
                } else {
                    // The store that was read is gone before the two were
                    // ever kept together: what follows from the new one
                    // coming after it has to be added here.
                    let mut edges = Vec::new();
                    StoreNodes::edges(&position.sc, &self.stores[written].sc, &mut edges);

                    for (from, to) in edges {
                        let added = seq_cst.add_edge(from, to);

                        debug_assert!(added, "[loom internal bug] SeqCst order is cyclic");
                    }

                    self.stores[written].take_position(position);
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
        self.mo_cache.clear();
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
        let cnt = cmp::min(self.cnt as usize, self.history);

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

            if !self.load_admissible(
                threads,
                seq_cst,
                atomic,
                i,
                &before,
                ordering,
                is_seq_cst(ordering),
            ) {
                // Returning this store contradicts the order of `SeqCst`
                // operations.
                continue 'outer;
            }

            // The load may return this store
            dst[n] = i as u8;
            n += 1;
        }

        assert!(
            n > 0 || self.cnt as usize <= self.history,
            "An atomic load can only return a store that loom does not keep anymore: more than \
             {} stores have been made to the atomic. Raise `Builder::atomic_history` \
             (`LOOM_ATOMIC_HISTORY`).",
            self.history
        );

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
        writes: impl Fn(u64) -> bool,
    ) -> usize {
        let mut n = 0;
        let cnt = cmp::min(self.cnt as usize, self.history);

        // A read-modify-write writes directly after the store it reads. Like
        // a load, it cannot read a store if the thread knows of a newer one:
        // what it writes comes after everything the thread knows of. It can
        // read a store that has newer stores the thread does not know of,
        // which then come after what it writes.
        let before = self.mo_before();

        'outer: for (i, store_i) in self.stores.iter().take(cnt).enumerate() {
            // An operation that does not write for the value it finds (a
            // compare-exchange that fails) is a load, and nothing more.
            let writes = writes(store_i.value);

            for (j, store_j) in self.stores.iter().take(cnt).enumerate() {
                if i == j {
                    continue;
                }

                let mo_i = store_i.modification_order;
                let mo_j = store_j.modification_order;

                assert_ne!(mo_i, mo_j);

                if writes && store_j.rmw_of == Some(i as u8) {
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

            let admissible = if writes {
                self.rmw_admissible(threads, seq_cst, atomic, i, &before, success, failure)
            } else {
                self.load_admissible(
                    threads,
                    seq_cst,
                    atomic,
                    i,
                    &before,
                    failure,
                    is_seq_cst(failure),
                )
            };

            if !admissible {
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

    /// The slot for the next store: a free one, or else the one of a store
    /// that is given up for it.
    ///
    /// The store that is given up must be one that no other store that is
    /// kept comes after in the modification order `before`. What is known
    /// about the order of the stores is known through each other: with a
    /// store in the middle gone, the ones before it could end up after the
    /// ones that followed it, and a thread that had seen it could go back
    /// to reading what came before. With an earliest store gone, all that
    /// is lost is that it can be read. Of the earliest stores, the one made
    /// first goes.
    fn next_slot(&self, before: &MoOrder, seq_cst: &SeqCst, read_by_rmw: Option<usize>) -> usize {
        let cnt = self.cnt as usize;

        if cnt < self.history {
            return cnt;
        }

        let mut first: Vec<usize> = (0..self.history)
            .filter(|&slot| before[slot] == 0)
            .collect();

        first.sort_by_key(|&slot| self.stores[slot].seq);

        // A store that no other store is known to come before may still
        // have to come after one: the order of `SeqCst` operations can say
        // so before the modification order does. Giving up such a store
        // would lose the value that the atomic ends up with. A store that
        // another one stands for from here on (see `forget_slot`) is not
        // taken to be first, and can go either way.
        let can_go = |slot: usize| {
            let stood_for = read_by_rmw == Some(slot)
                || (0..self.history).any(|other| self.stores[other].rmw_of == Some(slot as u8));

            if stood_for || seq_cst.is_empty() {
                return true;
            }

            let mut edges = Vec::new();

            for other in (0..self.history).filter(|&other| other != slot) {
                StoreNodes::edges(&self.stores[slot].sc, &self.stores[other].sc, &mut edges);
            }

            seq_cst.admits(None, &edges)
        };

        first
            .iter()
            .copied()
            .find(|&slot| can_go(slot))
            .or(first.first().copied())
            .expect("[loom internal bug] the modification order has no first store")
    }

    fn stores_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Store> {
        let cnt = cmp::min(self.cnt as usize, self.history);

        self.stores[..cnt].iter_mut()
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
fn slots(set: StoreSet) -> impl Iterator<Item = usize> {
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
            seq: 0,
            stands_for: Vec::new(),
        }
    }
}

impl Store {
    /// Where the store is in the modification order as far as its version
    /// goes, the threads that have seen it, and what counts as the store in
    /// the order of `SeqCst` operations.
    fn position(&self) -> Position {
        let mut stands_for = self.stands_for.clone();
        stands_for.push(self.happens_before);

        Position {
            modification_order: self.modification_order,
            first_seen: self.first_seen.clone(),
            sc: self.sc.clone(),
            stands_for,
        }
    }

    /// Takes the place of a store that this one directly follows, and that
    /// is being forgotten.
    fn take_position(&mut self, position: Position) {
        self.modification_order = position.modification_order;
        self.first_seen.merge(&position.first_seen);
        self.sc.absorb(&position.sc);
        self.stands_for.extend(position.stands_for);
    }
}

/// What a store hands to the one that stands for it once it is forgotten.
struct Position {
    modification_order: VersionVec,
    first_seen: FirstSeen,
    sc: StoreNodes,

    /// What the threads that made the store, and the ones it stood for
    /// itself, knew of at the time.
    stands_for: Vec<VersionVec>,
}

// ===== impl FirstSeen =====

impl FirstSeen {
    fn new() -> FirstSeen {
        FirstSeen([u16::MAX; MAX_THREADS])
    }

    /// Adds what `other` has been seen by.
    fn merge(&mut self, other: &FirstSeen) {
        for (seen, other) in self.0.iter_mut().zip(&other.0) {
            *seen = cmp::min(*seen, *other);
        }
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
