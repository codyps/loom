use crate::rt;
use crate::rt::{Access, Execution, Location, VersionVec};

use std::fmt;
use std::marker::PhantomData;

use tracing::trace;

#[cfg(feature = "checkpoint")]
use serde::{Deserialize, Serialize};

/// Stores objects
#[derive(Debug)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(super) struct Store<T = Entry> {
    /// Stored state for all objects.
    entries: Vec<T>,

    /// Identifies this store, and this generation of its contents: it changes
    /// whenever the store is cleared. References remember the id of the store
    /// they were created by, which is how a reference that outlived its
    /// execution is told apart from a valid one.
    id: StoreId,

    /// Index of the object tracking the `SeqCst` order, once there is one.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    seq_cst_fences: Option<usize>,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(super) struct StoreId(usize);

pub(super) trait Object: Sized {
    type Entry;

    /// Convert an object into an entry
    fn into_entry(self) -> Self::Entry;

    /// Convert an entry ref into an object ref
    fn get_ref(entry: &Self::Entry) -> Option<&Self>;

    /// Convert a mutable entry ref into a mutable object ref
    fn get_mut(entry: &mut Self::Entry) -> Option<&mut Self>;
}

/// References an object in the store.
///
/// The reference tracks the type it references. Using `()` indicates the type
/// is unknown.
#[derive(Eq, PartialEq)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(super) struct Ref<T = ()> {
    /// Index in the store
    index: usize,

    /// The store (generation) this reference belongs to.
    store_id: StoreId,

    _p: PhantomData<T>,
}

// TODO: mov to separate file
#[derive(Debug, Copy, Clone)]
pub(super) struct Operation {
    obj: Ref,
    action: Action,
    location: Location,
}

// TODO: move to separate file
#[derive(Debug, Copy, Clone, PartialEq)]
pub(super) enum Action {
    /// Action on an Arc object
    Arc(rt::arc::Action),

    /// Action on an atomic object
    Atomic(rt::atomic::Action),

    /// Action on a channel
    Channel(rt::mpsc::Action),

    /// Action on a RwLock
    RwLock(rt::rwlock::Action),

    /// Generic action with no specialized dependencies on access.
    Opaque,

    /// Like `Opaque`, but for an attempt that never waits for the object
    /// (e.g. `Mutex::try_lock`). A thread about to take such an action must
    /// not be blocked when another thread acquires the object first.
    OpaqueNonBlocking,

    /// Giving up an object held by the thread (e.g. unlocking a `Mutex`).
    /// This never blocks either. It only exists as an action so that an
    /// attempt on the object by another thread can be scheduled *before* it,
    /// i.e. while the object is still held.
    Release,
}

macro_rules! objects {
    ( $(#[$attrs:meta])* $e:ident, $( $name:ident($ty:path), )* ) => {

        $(#[$attrs])*
        pub(super) enum $e {

            $(
                $name($ty),
            )*
        }

        $(
            impl crate::rt::object::Object for $ty {
                type Entry = $e;

                fn into_entry(self) -> Entry {
                    $e::$name(self)
                }

                fn get_ref(entry: &Entry) -> Option<&$ty> {
                    match entry {
                        $e::$name(obj) => Some(obj),
                        _ => None,
                    }
                }

                fn get_mut(entry: &mut Entry) -> Option<&mut $ty> {
                    match entry {
                        $e::$name(obj) => Some(obj),
                        _ => None,
                    }
                }
            }
        )*
    };
}

objects! {
    #[derive(Debug)]
    // Many of the common variants of this enum are quite large --- only `Entry`
    // and `Alloc` are significantly smaller than most other variants.
    #[allow(clippy::large_enum_variant)]
    Entry,

    // State tracking allocations. Used for leak detection.
    Alloc(rt::alloc::State),

    // State associated with a modeled `Arc`.
    Arc(rt::arc::State),

    // State associated with an atomic cell
    Atomic(rt::atomic::State),

    // State associated with a mutex.
    Mutex(rt::mutex::State),

    // State associated with a modeled condvar.
    Condvar(rt::condvar::State),

    // State associated with a modeled thread notifier.
    Notify(rt::notify::State),

    // State associated with an RwLock
    RwLock(rt::rwlock::State),

    // State associated with a modeled channel.
    Channel(rt::mpsc::State),

    // Tracks access to a memory cell
    Cell(rt::cell::State),

    // The order of `SeqCst` fences
    SeqCstFences(rt::atomic::SeqCstFences),
}

impl<T> Store<T> {
    /// Create a new, empty, object store
    pub(super) fn with_capacity(capacity: usize) -> Store<T> {
        Store {
            entries: Vec::with_capacity(capacity),
            id: StoreId::new(),
            seq_cst_fences: None,
        }
    }

    pub(super) fn id(&self) -> StoreId {
        self.id
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The index the next inserted object will get.
    pub(super) fn next_index(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    pub(super) fn reserve_exact(&mut self, additional: usize) {
        self.entries.reserve_exact(additional);
    }

    /// Insert an object into the store
    pub(super) fn insert<O>(&mut self, item: O) -> Ref<O>
    where
        O: Object<Entry = T>,
    {
        let index = self.entries.len();
        self.entries.push(item.into_entry());

        Ref {
            index,
            store_id: self.id,
            _p: PhantomData,
        }
    }

    pub(crate) fn truncate<O>(&mut self, obj: Ref<O>) {
        let target = obj.index + 1;
        self.entries.truncate(target);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.id = StoreId::new();
        self.seq_cst_fences = None;
    }

    pub(super) fn iter_ref<O>(&self) -> impl DoubleEndedIterator<Item = Ref<O>> + '_
    where
        O: Object<Entry = T>,
    {
        let store_id = self.id;

        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| O::get_ref(e).is_some())
            .map(move |(index, _)| Ref {
                index,
                store_id,
                _p: PhantomData,
            })
    }

    pub(super) fn iter_mut<'a, O>(&'a mut self) -> impl DoubleEndedIterator<Item = &'a mut O>
    where
        O: Object<Entry = T> + 'a,
    {
        self.entries.iter_mut().filter_map(O::get_mut)
    }
}

impl Store {
    pub(super) fn seq_cst_fences(&self) -> Option<Ref<rt::atomic::SeqCstFences>> {
        self.seq_cst_fences.map(|index| Ref {
            index,
            store_id: self.id,
            _p: PhantomData,
        })
    }

    pub(super) fn set_seq_cst_fences(&mut self, fences: Ref<rt::atomic::SeqCstFences>) {
        self.seq_cst_fences = Some(fences.index);
    }

    fn seq_cst_fences_entry(&self) -> Option<&rt::atomic::SeqCstFences> {
        match self.entries.get(self.seq_cst_fences?) {
            Some(Entry::SeqCstFences(fences)) => Some(fences),
            _ => None,
        }
    }

    /// Calls `f` with every previous access that `operation` is dependent
    /// with and that could still be unordered with respect to it.
    ///
    /// With `racing` set, accesses that `operation` depends on but can never
    /// be reordered with are left out. Those matter for tracking what the
    /// operation is ordered after, but are not places to backtrack to.
    pub(super) fn for_each_dependent_access(
        &self,
        operation: Operation,
        racing: bool,
        mut f: impl FnMut(&Access),
    ) {
        let last = match &self.entries[operation.obj.index] {
            Entry::Atomic(entry) => {
                let action: rt::atomic::Action = operation.action.into();

                // A `SeqCst` access is also ordered with `SeqCst` fences.
                if action.is_seq_cst() {
                    let fence = self
                        .seq_cst_fences_entry()
                        .and_then(|fences| fences.last_fence_access());

                    if let Some(access) = fence {
                        f(access);
                    }
                }

                entry.dependent_accesses(action).for_each(f);
                return;
            }
            Entry::SeqCstFences(entry) => {
                entry.dependent_accesses().for_each(f);
                return;
            }
            Entry::Mutex(entry) => {
                entry
                    .dependent_accesses(operation.action, racing)
                    .for_each(f);
                return;
            }
            Entry::RwLock(entry) => {
                entry
                    .dependent_accesses(operation.action, racing)
                    .for_each(f);
                return;
            }
            Entry::Channel(entry) => {
                entry
                    .dependent_accesses(operation.action.into())
                    .for_each(f);
                return;
            }
            Entry::Arc(entry) => entry.last_dependent_access(operation.action.into()),
            Entry::Condvar(entry) => entry.last_dependent_access(),
            Entry::Notify(entry) => entry.last_dependent_access(),
            obj => panic!(
                "object is not branchable {:?}; ref = {:?}",
                obj, operation.obj
            ),
        };

        if let Some(access) = last {
            f(access);
        }
    }

    pub(super) fn set_last_access(
        &mut self,
        operation: Operation,
        thread: usize,
        path_id: usize,
        dpor_vv: &VersionVec,
    ) {
        if let Entry::Atomic(_) = &self.entries[operation.obj.index] {
            let action: rt::atomic::Action = operation.action.into();

            if action.is_seq_cst() {
                if let Some(Entry::SeqCstFences(fences)) = self
                    .seq_cst_fences
                    .and_then(|index| self.entries.get_mut(index))
                {
                    fences.set_last_sc_access(thread, path_id, dpor_vv);
                }
            }
        }

        match &mut self.entries[operation.obj.index] {
            Entry::Arc(entry) => entry.set_last_access(operation.action.into(), path_id, dpor_vv),
            Entry::Atomic(entry) => {
                entry.set_last_access(operation.action.into(), thread, path_id, dpor_vv)
            }
            Entry::Mutex(entry) => entry.set_last_access(operation.action, path_id, dpor_vv),
            Entry::Condvar(entry) => entry.set_last_access(path_id, dpor_vv),
            Entry::Notify(entry) => entry.set_last_access(path_id, dpor_vv),
            Entry::SeqCstFences(entry) => entry.set_last_access(path_id, dpor_vv),
            Entry::RwLock(entry) => entry.set_last_access(operation.action, path_id, dpor_vv),
            Entry::Channel(entry) => {
                entry.set_last_access(operation.action.into(), path_id, dpor_vv)
            }
            _ => panic!("object is not branchable"),
        }
    }

    /// Panics if any leaks were detected
    pub(crate) fn check_for_leaks(&self) {
        for (index, entry) in self.entries.iter().enumerate() {
            match entry {
                Entry::Alloc(entry) => entry.check_for_leaks(index),
                Entry::Arc(entry) => entry.check_for_leaks(index),
                Entry::Channel(entry) => entry.check_for_leaks(index),
                _ => {}
            }
        }
    }
}

impl<T> Ref<T> {
    /// Erase the type marker
    pub(super) fn erase(self) -> Ref<()> {
        Ref {
            index: self.index,
            store_id: self.store_id,
            _p: PhantomData,
        }
    }

    pub(super) fn ref_eq(self, other: Ref<T>) -> bool {
        self.index == other.index
    }

    /// The position of the object in its store, which identifies it within
    /// an execution.
    pub(super) fn index(self) -> usize {
        self.index
    }
}

impl StoreId {
    fn new() -> StoreId {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering::Relaxed;

        // The number picked here is arbitrary. It is mostly to avoid collision
        // with "zero" to aid with debugging.
        static NEXT_ID: AtomicUsize = AtomicUsize::new(41_123_456);

        StoreId(NEXT_ID.fetch_add(1, Relaxed))
    }
}

impl<T: Object> Ref<T> {
    /// Panics if the reference was not created by (the current generation of)
    /// `store`.
    #[track_caller]
    fn assert_store(self, store: &Store<T::Entry>) {
        assert!(
            self.store_id == store.id,
            "Tried to access a loom object that does not belong to the current execution. \
             Loom objects must not be reused across executions of a model: is one being \
             kept in a `static`, a `std::thread_local!`, or otherwise outside the closure \
             passed to `loom::model`?"
        );
    }

    /// Get a reference to the object associated with this reference from the store
    pub(super) fn get(self, store: &Store<T::Entry>) -> &T {
        self.assert_store(store);
        T::get_ref(&store.entries[self.index])
            .expect("[loom internal bug] unexpected object stored at reference")
    }

    /// Like `get`, but returns `None` if the reference does not point at an
    /// object of this type, e.g. because it outlived the execution it was
    /// created in.
    pub(super) fn try_get(self, store: &Store<T::Entry>) -> Option<&T> {
        if self.store_id != store.id {
            return None;
        }

        store.entries.get(self.index).and_then(T::get_ref)
    }

    /// Like `get_mut`, but returns `None` where `try_get` does.
    pub(super) fn try_get_mut(self, store: &mut Store<T::Entry>) -> Option<&mut T> {
        if self.store_id != store.id {
            return None;
        }

        store.entries.get_mut(self.index).and_then(T::get_mut)
    }

    /// Get a mutable reference to the object associated with this reference
    /// from the store
    pub(super) fn get_mut(self, store: &mut Store<T::Entry>) -> &mut T {
        self.assert_store(store);
        T::get_mut(&mut store.entries[self.index])
            .expect("[loom internal bug] unexpected object stored at reference")
    }
}

impl Ref {
    /// Convert a store index `usize` into a ref
    pub(super) fn from_usize(index: usize, store_id: StoreId) -> Ref {
        Ref {
            index,
            store_id,
            _p: PhantomData,
        }
    }

    pub(super) fn downcast<T>(self, store: &Store<T::Entry>) -> Option<Ref<T>>
    where
        T: Object,
    {
        T::get_ref(&store.entries[self.index]).map(|_| Ref {
            index: self.index,
            store_id: self.store_id,
            _p: PhantomData,
        })
    }
}

impl<T> Clone for Ref<T> {
    fn clone(&self) -> Ref<T> {
        Ref {
            index: self.index,
            store_id: self.store_id,
            _p: PhantomData,
        }
    }
}

impl<T> Copy for Ref<T> {}

impl<T> fmt::Debug for Ref<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        use std::any::type_name;

        write!(fmt, "Ref<{}>({})", type_name::<T>(), self.index)
    }
}

// TODO: These fns shouldn't be on Ref
impl<T: Object<Entry = Entry>> Ref<T> {
    // TODO: rename `branch_disable`
    pub(super) fn branch_acquire(self, is_locked: bool, location: Location) {
        super::branch(|execution| {
            trace!(obj = ?self, ?is_locked, %location, "Object::branch_acquire");

            self.set_action(execution, Action::Opaque, location);

            if is_locked {
                // The mutex is currently blocked, cannot make progress
                execution.threads.active_mut().set_blocked(location);
            }
        })
    }

    pub(super) fn branch_action(
        self,
        action: impl Into<Action> + std::fmt::Debug,
        location: Location,
    ) {
        super::branch(|execution| {
            trace!(obj = ?self, ?action, %location, "Object::branch_action");

            self.set_action(execution, action.into(), location);
        })
    }

    pub(super) fn branch_disable(
        self,
        action: impl Into<Action> + std::fmt::Debug,
        disable: bool,
        location: Location,
    ) {
        super::branch(|execution| {
            trace!(obj = ?self, ?action, ?disable, %location, "Object::branch_disable");

            self.set_action(execution, action.into(), location);

            if disable {
                // Cannot make progress.
                execution.threads.active_mut().set_blocked(location);
            }
        })
    }

    pub(super) fn branch_opaque(self, location: Location) {
        self.branch_action(Action::Opaque, location)
    }

    pub(super) fn branch_opaque_non_blocking(self, location: Location) {
        self.branch_action(Action::OpaqueNonBlocking, location)
    }

    pub(super) fn branch_release(self, location: Location) {
        self.branch_action(Action::Release, location)
    }

    fn set_action(self, execution: &mut Execution, action: Action, location: Location) {
        self.assert_store(&execution.objects);

        assert!(
            T::get_ref(&execution.objects.entries[self.index]).is_some(),
            "failed to get object for ref {:?}",
            self
        );

        execution.threads.active_mut().operation = Some(Operation {
            obj: self.erase(),
            action,
            location,
        });
    }
}

impl Operation {
    pub(super) fn object(&self) -> Ref {
        self.obj
    }

    pub(super) fn action(&self) -> Action {
        self.action
    }

    pub(super) fn location(&self) -> Location {
        self.location
    }
}

impl From<Action> for rt::arc::Action {
    fn from(action: Action) -> Self {
        match action {
            Action::Arc(action) => action,
            _ => unreachable!(),
        }
    }
}

impl From<Action> for rt::atomic::Action {
    fn from(action: Action) -> Self {
        match action {
            Action::Atomic(action) => action,
            _ => unreachable!(),
        }
    }
}

impl From<Action> for rt::mpsc::Action {
    fn from(action: Action) -> Self {
        match action {
            Action::Channel(action) => action,
            _ => unreachable!(),
        }
    }
}

impl From<rt::arc::Action> for Action {
    fn from(action: rt::arc::Action) -> Self {
        Action::Arc(action)
    }
}

impl From<rt::atomic::Action> for Action {
    fn from(action: rt::atomic::Action) -> Self {
        Action::Atomic(action)
    }
}

impl From<rt::mpsc::Action> for Action {
    fn from(action: rt::mpsc::Action) -> Self {
        Action::Channel(action)
    }
}

impl From<rt::rwlock::Action> for Action {
    fn from(action: rt::rwlock::Action) -> Self {
        Action::RwLock(action)
    }
}

impl PartialEq<rt::rwlock::Action> for Action {
    fn eq(&self, other: &rt::rwlock::Action) -> bool {
        let other: Action = (*other).into();
        *self == other
    }
}
