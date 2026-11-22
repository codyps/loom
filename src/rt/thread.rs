use crate::rt::execution;
use crate::rt::object::Operation;
use crate::rt::seq_cst;
use crate::rt::vv::VersionVec;

use std::{any::Any, collections::HashMap, fmt, ops};

use super::Location;
pub(crate) struct Thread {
    pub id: Id,

    /// If the thread is runnable, blocked, or terminated.
    pub state: State,

    /// True if the thread is in a critical section
    pub critical: bool,

    /// True while the thread runs its body inside `speculation::run_thread`,
    /// which catches what an execution that is being dropped unwinds with.
    pub(crate) in_body: bool,

    /// The operation the thread is about to take
    pub(super) operation: Option<Operation>,

    /// True while the thread is blocked inside `thread::park` itself — the
    /// only blocked state that [`Set::unpark`] may wake. A thread blocked on
    /// any *other* operation (a `JoinHandle::join`'s notify, a lock) is not
    /// parked: an `unpark` aimed at it banks [`Thread::park_token`] instead,
    /// exactly as `std::thread::Thread::unpark` guarantees.
    pub(super) blocked_in_park: bool,

    /// A pending `unpark` token (std's park/unpark "token" semantics): made
    /// available by [`Set::unpark`] while the thread is not blocked in
    /// `park`; consumed by the thread's next `park` call, which then returns
    /// immediately instead of blocking.
    pub(super) park_token: bool,

    /// Tracks observed causality
    pub causality: VersionVec,

    /// Tracks the view of the lastest release fence
    pub released: VersionVec,

    /// Tracks DPOR relations
    pub dpor_vv: VersionVec,

    /// The location (atomic, or fence) of the thread's last atomic operation,
    /// what the thread knew of after it, and what it knew of after its last
    /// operation on a different location before that. The order of `SeqCst`
    /// operations depends on the latter, see `rt::seq_cst`.
    last_location: Option<usize>,
    last_view: VersionVec,
    elsewhere_view: VersionVec,

    /// Version at which the thread last yielded
    pub last_yield: Option<u16>,

    /// Number of times the thread yielded
    pub yield_count: usize,

    /// True if the thread has done something other than an atomic operation
    /// since its last atomic operation.
    other_pending: bool,

    /// True from when the thread yields until it runs again. When it can do
    /// that depends on what the other threads do, whatever that is.
    pub(super) yielded: bool,

    /// The position in the path of the scheduling decision that the thread
    /// last ran from, if it has run.
    pub(super) last_step: Option<usize>,

    /// The threads (as a bit set) that the thread, having yielded, waits
    /// for: it does not run again before each of them has taken a step or
    /// has become unable to.
    pub(super) yield_waits: crate::rt::ThreadSet,

    /// The memory the thread was handed by its consume loads, see
    /// [`Thread::consume`].
    consumed: Vec<Consumed>,

    /// Set while the thread makes an access that carries a dependency from
    /// a consume load, and that orders nothing but itself after the store
    /// that was loaded, see [`Thread::depend`].
    dependent: Option<Dependent>,

    locals: LocalMap,

    /// `tracing` span used to associate diagnostics with the current thread.
    span: tracing::Span,
}

/// What a consume load returned a pointer to: `len` bytes at `addr`, and
/// what the store that the load read releases. An access by the thread to
/// an object in there carries a dependency from the load, and so comes
/// after all of `clock`.
/// An access that carries a dependency from a consume load.
#[derive(Debug)]
struct Dependent {
    /// What the thread knows of apart from the dependency: what it knew of
    /// before the access, and what the access itself has acquired since.
    /// `causality` is back to this when the access is over.
    known: VersionVec,

    /// What the access comes after by the dependency.
    clock: VersionVec,
}

#[derive(Debug)]
struct Consumed {
    addr: usize,
    len: usize,
    clock: VersionVec,
}

#[derive(Debug)]
pub(crate) struct Set {
    /// Unique execution identifier
    execution_id: execution::Id,

    /// Set of threads
    threads: Vec<Thread>,

    /// Currently scheduled thread.
    ///
    /// `None` signifies that no thread is runnable.
    active: Option<usize>,

    /// `tracing` span used as the parent for new thread spans.
    iteration_span: tracing::Span,
}

#[derive(Eq, PartialEq, Hash, Copy, Clone)]
pub(crate) struct Id {
    execution_id: execution::Id,
    id: usize,
}

impl Id {
    /// Returns an integer ID unique to this current execution (for use in
    /// [`thread::ThreadId`]'s `Debug` impl)
    pub(crate) fn public_id(&self) -> usize {
        self.id
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum State {
    Runnable,
    Blocked(#[allow(dead_code)] Location),
    Yield,
    Terminated,
}

type LocalMap = HashMap<LocalKeyId, LocalValue>;

#[derive(Eq, PartialEq, Hash, Copy, Clone)]
struct LocalKeyId(usize);

struct LocalValue(Option<Box<dyn Any>>);

impl Thread {
    fn new(id: Id, parent_span: &tracing::Span) -> Thread {
        Thread {
            id,
            span: tracing::info_span!(parent: parent_span.id(), "thread", id = id.id),
            state: State::Runnable,
            critical: false,
            in_body: false,
            operation: None,
            blocked_in_park: false,
            park_token: false,
            causality: VersionVec::new(),
            released: VersionVec::new(),
            dpor_vv: VersionVec::new(),
            last_location: None,
            last_view: VersionVec::new(),
            elsewhere_view: VersionVec::new(),
            last_yield: None,
            yield_count: 0,
            yield_waits: 0,
            other_pending: false,
            yielded: false,
            last_step: None,
            consumed: Vec::new(),
            dependent: None,
            locals: HashMap::new(),
        }
    }

    /// What the thread knew of after its last atomic operation on a location
    /// other than `location`.
    pub(crate) fn elsewhere(&self, location: usize) -> VersionVec {
        match self.last_location {
            // A fence is not a location: two fences are not "the same".
            Some(last) if last == location && location != seq_cst::NO_LOCATION => {
                self.elsewhere_view
            }
            Some(_) => self.last_view,
            None => VersionVec::new(),
        }
    }

    /// A consume load by the thread returned a pointer to `len` bytes at
    /// `addr`, from a store that releases `clock`.
    ///
    /// The load orders nothing by itself. Only what the thread then does
    /// with the pointer comes after the store, see [`Thread::dependency`].
    pub(crate) fn consume(&mut self, addr: usize, len: usize, mut clock: VersionVec) {
        // A zero-sized pointee still has an address to be found at.
        let len = len.max(1);

        // The atomic was itself got to through a consumed pointer: what is
        // done with the pointer that was loaded depends on that one too.
        if let Some(dependent) = &self.dependent {
            clock.join(&dependent.clock);
        }

        // Memory that overlaps an earlier pointee is that pointee again, or
        // was allocated after it was freed: the earlier load says nothing
        // about it anymore.
        //
        // The ends saturate: the pointer may be any value, not only one to
        // memory that exists.
        self.consumed
            .retain(|c| c.addr >= addr.saturating_add(len) || addr >= c.addr.saturating_add(c.len));
        self.consumed.push(Consumed { addr, len, clock });
    }

    /// The thread is about to access the loom object at `addr`, which was
    /// created at `created`. If the thread got to it through a pointer from
    /// one of its consume loads, the access carries a dependency from the
    /// load, and comes after what the store that was loaded releases: that
    /// is returned, unless the thread knows of all of it anyway.
    ///
    /// Whether it did is not known, values are not followed. It is taken to
    /// have done so if the object is:
    ///
    /// - in the memory the pointer points to, or
    /// - not known to the thread, having been created after everything the
    ///   thread knows of, but before the store. Some pointer led the thread
    ///   to the object, and it did not get that one by synchronizing. This
    ///   is how an object is found that is behind a second pointer, such as
    ///   a `Box` in the pointee.
    pub(crate) fn dependency(&self, addr: usize, created: &VersionVec) -> Option<VersionVec> {
        let mut after = self.causality;

        for c in &self.consumed {
            let inside = addr >= c.addr && addr - c.addr < c.len;
            // The first load that explains how the thread found the object
            // is the one it is taken to have found it by.
            let found_by = || after.ahead(created).is_some() && c.clock.ahead(created).is_none();

            if inside || found_by() {
                after.join(&c.clock);
            }
        }

        if after == self.causality {
            None
        } else {
            Some(after)
        }
    }

    /// The thread starts an atomic operation on the object at `addr`, see
    /// [`Thread::dependency`]. If the operation carries a dependency, the
    /// thread comes after what that orders it after.
    ///
    /// Only the operation itself comes after it, and not what the thread
    /// does next without the pointer: unless `lasting`, the thread forgets
    /// about it when the operation is over, in [`Thread::settle_dependent`].
    /// It keeps what the operation acquires by itself.
    ///
    /// A `lasting` dependency is more than a consume load provides. It is
    /// for operations that leave something behind that the thread comes
    /// after from then on, and that says what the operation knew of: a
    /// store, or a place in the order of `SeqCst` operations.
    pub(crate) fn depend(&mut self, addr: usize, created: &VersionVec, lasting: bool) {
        debug_assert!(self.dependent.is_none());

        if let Some(after) = self.dependency(addr, created) {
            if !lasting {
                self.dependent = Some(Dependent {
                    known: self.causality,
                    clock: after,
                });
            }

            self.causality = after;
        }
    }

    /// The operation that [`Thread::depend`] was called for is over.
    pub(crate) fn settle_dependent(&mut self) {
        if let Some(dependent) = self.dependent.take() {
            self.causality = dependent.known;
        }
    }

    /// The thread acquires `clock`, for good.
    pub(crate) fn acquire(&mut self, clock: &VersionVec) {
        self.causality.join(clock);

        if let Some(dependent) = &mut self.dependent {
            dependent.known.join(clock);
        }
    }

    /// What the thread knows of, and goes on knowing of: `causality`, less
    /// what an access knows of only by a dependency from a consume load.
    /// This is what to record of an access that later ones are checked
    /// against.
    pub(crate) fn known(&self) -> &VersionVec {
        match &self.dependent {
            Some(dependent) => &dependent.known,
            None => &self.causality,
        }
    }

    /// Records that the thread is about to do something that is not an
    /// atomic operation, such as taking a lock.
    pub(crate) fn note_other(&mut self) {
        self.other_pending = true;
    }

    /// Called when the thread gets to an atomic operation: whatever else it
    /// has done since the last one is over, and counts as an operation on a
    /// location of its own.
    pub(crate) fn settle_other(&mut self) {
        if self.other_pending {
            self.other_pending = false;
            self.note_access(seq_cst::OTHER_LOCATION);
        }
    }

    /// Records that the thread has executed an atomic operation on
    /// `location`.
    pub(crate) fn note_access(&mut self, location: usize) {
        if self.last_location != Some(location) || location == seq_cst::NO_LOCATION {
            if self.last_location.is_some() {
                self.elsewhere_view = self.last_view;
            }

            self.last_location = Some(location);
        }

        self.last_view = *self.known();
    }

    pub(crate) fn is_runnable(&self) -> bool {
        matches!(self.state, State::Runnable { .. })
    }

    pub(crate) fn set_runnable(&mut self) {
        self.state = State::Runnable;
    }

    pub(crate) fn set_blocked(&mut self, location: Location) {
        self.state = State::Blocked(location);
    }

    pub(crate) fn is_blocked(&self) -> bool {
        matches!(self.state, State::Blocked(..))
    }

    pub(crate) fn is_yield(&self) -> bool {
        matches!(self.state, State::Yield)
    }

    pub(crate) fn set_yield(&mut self) {
        self.state = State::Yield;
        self.yielded = true;
        self.last_yield = Some(self.causality[self.id]);
        self.yield_count += 1;
    }

    pub(crate) fn is_terminated(&self) -> bool {
        matches!(self.state, State::Terminated)
    }

    pub(crate) fn set_terminated(&mut self) {
        self.state = State::Terminated;
    }

    pub(crate) fn drop_locals(&mut self) -> Box<dyn std::any::Any> {
        let mut locals = Vec::with_capacity(self.locals.len());

        // run the Drop impls of any mock thread-locals created by this thread.
        for local in self.locals.values_mut() {
            locals.push(local.0.take());
        }

        Box::new(locals)
    }

    /// Wake a thread that is blocked (or spuriously yielded) on the caller's
    /// object operation — a `Notify` wait, which also underlies
    /// `JoinHandle::join`. Callers must have verified the thread is waiting
    /// on *their* object. This is an object-directed wake, not
    /// `Thread::unpark`: park-token semantics for the public unpark API live
    /// in [`Set::unpark`].
    pub(crate) fn wake(&mut self, waker: &Thread) {
        self.causality.join(&waker.causality);
        if self.is_blocked() || self.is_yield() {
            self.blocked_in_park = false;
            self.set_runnable();
        }
    }
}

impl fmt::Debug for Thread {
    // Manual debug impl is necessary because thread locals are represented as
    // `dyn Any`, which does not implement `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Thread")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("critical", &self.critical)
            .field("operation", &self.operation)
            .field("causality", &self.causality)
            .field("released", &self.released)
            .field("dpor_vv", &self.dpor_vv)
            .field("last_yield", &self.last_yield)
            .field("yield_count", &self.yield_count)
            .field("locals", &format_args!("[..locals..]"))
            .finish()
    }
}

impl Set {
    /// Create an empty thread set.
    ///
    /// The set may contain up to `max_threads` threads.
    pub(crate) fn new(execution_id: execution::Id, max_threads: usize) -> Set {
        let mut threads = Vec::with_capacity(max_threads);
        // Capture the current iteration's span to be used as each thread
        // span's parent.
        let iteration_span = tracing::Span::current();
        // Push initial thread
        threads.push(Thread::new(Id::new(execution_id, 0), &iteration_span));

        Set {
            execution_id,
            threads,
            active: Some(0),
            iteration_span,
        }
    }

    pub(crate) fn execution_id(&self) -> execution::Id {
        self.execution_id
    }

    /// Create a new thread
    pub(crate) fn new_thread(&mut self) -> Id {
        assert!(
            self.threads.len() < self.max(),
            "the model has more than {} threads, counting the main one (see `max_threads`)",
            self.max()
        );

        // Get the identifier for the thread about to be created
        let id = self.threads.len();

        // Push the thread onto the stack
        self.threads.push(Thread::new(
            Id::new(self.execution_id, id),
            &self.iteration_span,
        ));

        Id::new(self.execution_id, id)
    }

    pub(crate) fn max(&self) -> usize {
        self.threads.capacity()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn is_complete(&self) -> bool {
        if self.active.is_none() {
            // All threads should be terminated
            for thread in &self.threads {
                assert!(
                    thread.is_terminated(),
                    "thread not terminated; {:#?}",
                    thread
                );
            }

            true
        } else {
            false
        }
    }

    pub(crate) fn active_id(&self) -> Id {
        Id::new(self.execution_id, self.active.unwrap())
    }

    pub(crate) fn active(&self) -> &Thread {
        &self.threads[self.active.unwrap()]
    }

    pub(crate) fn set_active(&mut self, id: Option<Id>) {
        tracing::dispatcher::get_default(|subscriber| {
            if let Some(span_id) = self.active().span.id() {
                subscriber.exit(&span_id)
            }

            if let Some(span_id) = id.and_then(|id| self.threads.get(id.id)?.span.id()) {
                subscriber.enter(&span_id);
            }
        });
        self.active = id.map(Id::as_usize);
    }

    pub(crate) fn active_mut(&mut self) -> &mut Thread {
        &mut self.threads[self.active.unwrap()]
    }

    /// Get the active thread and second thread
    pub(crate) fn active2_mut(&mut self, other: Id) -> (&mut Thread, &mut Thread) {
        let active = self.active.unwrap();
        let other = other.id;

        if other >= active {
            let (l, r) = self.threads.split_at_mut(other);

            (&mut l[active], &mut r[0])
        } else {
            let (l, r) = self.threads.split_at_mut(active);

            (&mut r[0], &mut l[other])
        }
    }

    pub(crate) fn active_causality_inc(&mut self) {
        let id = self.active_id();
        let active = self.active_mut();

        active.causality.inc(id);

        if let Some(dependent) = &mut active.dependent {
            dependent.known.inc(id);
        }
    }

    pub(crate) fn active_atomic_version(&self) -> u16 {
        let id = self.active_id();
        self.active().causality[id]
    }

    /// `Thread::unpark`, with std's token semantics: only a thread blocked
    /// inside `thread::park` itself is woken. A thread blocked on any other
    /// operation — a `JoinHandle::join`, a `Condvar` wait it has not reached
    /// the internal `park` of yet, a lock — is left waiting and the token is
    /// banked for its next `park`, which will then return immediately.
    /// Waking such a thread instead resumed it without the notification it
    /// was waiting for.
    pub(crate) fn unpark(&mut self, id: Id) {
        if id == self.active_id() {
            // The thread is unparking itself. It is running, so it cannot be
            // parked: bank the token. No causality join is needed since the
            // thread *is* the unparker.
            self.active_mut().park_token = true;
            return;
        }

        // Synchronize memory: the target's next `park` return (or immediate
        // token consumption) happens-after this unpark.
        let (active, th) = self.active2_mut(id);
        th.causality.join(&active.causality);

        if th.blocked_in_park {
            // Wake the park; the wakeup consumes the token.
            th.blocked_in_park = false;
            th.set_runnable();
        } else {
            th.park_token = true;
        }
    }

    /// Wakes a thread blocked with `rt::block`, which the caller must know
    /// to be waiting for it. The woken thread continues after the waker's
    /// current point.
    pub(crate) fn wake(&mut self, id: Id) {
        let (active, th) = self.active2_mut(id);
        th.wake(active);
    }

    /// Called by synchronization primitives that used to insert a point of
    /// sequential consistency here.
    ///
    /// This does nothing. Treating a `SeqCst` access (or a lock operation) as
    /// a point at which the thread exchanges everything it knows with a
    /// global view is far stronger than what `SeqCst` guarantees, and hides
    /// bugs. `SeqCst` loads and stores are ordered per atomic instead, see
    /// `rt::atomic`; locks need no more than acquire / release.
    pub(crate) fn seq_cst(&mut self) {}

    pub(crate) fn clear(&mut self, execution_id: execution::Id) {
        self.iteration_span = tracing::Span::current();
        self.threads.clear();
        self.threads
            .push(Thread::new(Id::new(execution_id, 0), &self.iteration_span));

        self.execution_id = execution_id;
        self.active = Some(0);
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (Id, &Thread)> + '_ {
        let execution_id = self.execution_id;
        self.threads
            .iter()
            .enumerate()
            .map(move |(id, thread)| (Id::new(execution_id, id), thread))
    }

    pub(crate) fn iter_mut(&mut self) -> impl ExactSizeIterator<Item = (Id, &mut Thread)> + '_ {
        let execution_id = self.execution_id;
        self.threads
            .iter_mut()
            .enumerate()
            .map(move |(id, thread)| (Id::new(execution_id, id), thread))
    }

    /// Split the set of threads into the active thread and an iterator of all
    /// other threads.
    pub(crate) fn split_active(&mut self) -> (&mut Thread, impl Iterator<Item = &mut Thread>) {
        let active = self.active.unwrap();
        let (one, two) = self.threads.split_at_mut(active);
        let (active, two) = two.split_at_mut(1);

        let iter = one.iter_mut().chain(two.iter_mut());

        (&mut active[0], iter)
    }

    pub(crate) fn local<T: 'static>(
        &mut self,
        key: &'static crate::thread::LocalKey<T>,
    ) -> Option<Result<&T, AccessError>> {
        self.active_mut()
            .locals
            .get(&LocalKeyId::new(key))
            .map(|local_value| local_value.get())
    }

    pub(crate) fn local_init<T: 'static>(
        &mut self,
        key: &'static crate::thread::LocalKey<T>,
        value: T,
    ) {
        assert!(self
            .active_mut()
            .locals
            .insert(LocalKeyId::new(key), LocalValue::new(value))
            .is_none())
    }
}

impl ops::Index<Id> for Set {
    type Output = Thread;

    fn index(&self, index: Id) -> &Thread {
        &self.threads[index.id]
    }
}

impl ops::IndexMut<Id> for Set {
    fn index_mut(&mut self, index: Id) -> &mut Thread {
        &mut self.threads[index.id]
    }
}

impl Id {
    pub(crate) fn new(execution_id: execution::Id, id: usize) -> Id {
        Id { execution_id, id }
    }

    pub(crate) fn as_usize(self) -> usize {
        self.id
    }
}

impl From<Id> for usize {
    fn from(src: Id) -> usize {
        src.id
    }
}

impl fmt::Display for Id {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.id.fmt(fmt)
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "Id({})", self.id)
    }
}

impl LocalKeyId {
    fn new<T>(key: &'static crate::thread::LocalKey<T>) -> Self {
        Self(key as *const _ as usize)
    }
}

impl LocalValue {
    fn new<T: 'static>(value: T) -> Self {
        Self(Some(Box::new(value)))
    }

    fn get<T: 'static>(&self) -> Result<&T, AccessError> {
        self.0
            .as_ref()
            .ok_or(AccessError { _private: () })
            .map(|val| {
                val.downcast_ref::<T>()
                    .expect("local value must downcast to expected type")
            })
    }
}

/// An error returned by [`LocalKey::try_with`](struct.LocalKey.html#method.try_with).
pub struct AccessError {
    _private: (),
}

impl fmt::Debug for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessError").finish()
    }
}

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt("already destroyed", f)
    }
}
