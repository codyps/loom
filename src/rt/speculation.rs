//! Load buffering: a `Relaxed` load that returns a store which has not been
//! made yet.
//!
//! RC11 requires program order and reads-from together to be acyclic, which
//! rules out a load reading a store that depends on what the loading thread
//! does after the load. The C++ standard, and hardware such as Arm, allow it.
//! An execution in loom runs one step after the other, so the only way to
//! give such a load its value is to guess it and check later. This is what
//! is done when `Builder::load_buffering` is set:
//!
//! - The model is explored twice. The first pass is the usual one, and
//!   records the values each atomic is given. In the second, a `Relaxed`
//!   load may also return one of those values that no store it could read
//!   has, as a guess. Taking guesses only from executions without any keeps
//!   out values that would justify themselves (out of thin air): a value
//!   copied back and forth between two atomics has to come from somewhere.
//! - A guess is settled by a store of the value to the atomic, by another
//!   thread, that does not happen after the load, where reading it is
//!   consistent with the modification order and the order of `SeqCst`
//!   operations. The load then counts as having read that store.
//! - Until then the loading thread may not access the atomic again, nor
//!   execute an acquire or `SeqCst` fence: both would depend on the store
//!   that was read. An execution in which it does is dropped, as is one that
//!   ends, or deadlocks, with a guess unsettled.
//! - A panic while a guess is unsettled is held back: whether it is a failure
//!   depends on whether the guess turns out to be right. The execution goes
//!   on without the panicking thread, and the panic is raised again at its
//!   end if every guess has been settled.
//!
//! This explores a subset of what RC11 without its no-thin-air axiom allows,
//! and nothing it forbids.

use crate::rt::seq_cst::NodeSet;
use crate::rt::{Execution, Scheduler, ThreadSet, VersionVec};

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

/// What a thread unwinds with when the execution is dropped: it is not one
/// that exists.
///
/// Dropping an execution unwinds each of its threads in turn, from the
/// inside: the one that finds out, right away, and every other one when it
/// is next scheduled (see `Execution::schedule` and `Scheduler::switch`).
/// Each thread's wrapper (`run_thread`) catches it, so the thread ends as it
/// would otherwise, and its coroutine is ready for the next execution:
/// cancelling coroutines from the outside is not safe while other tests
/// run. `resume_unwind` does not run the panic hook, so nothing is printed.
pub(crate) struct Discard;

/// Drops the current execution, starting with the current thread, unless it
/// is unwinding already: a second panic would abort the process. It carries
/// on then, and the execution is dropped all the same.
pub(crate) fn discard(execution: &mut Execution) {
    if let Some(load_buffering) = &mut execution.load_buffering {
        load_buffering.discarding = true;
    }

    if !std::thread::panicking() {
        resume_unwind(Box::new(Discard));
    }
}

/// Returns `true` if the execution is being dropped.
pub(crate) fn is_discarding(execution: &Execution) -> bool {
    execution
        .load_buffering
        .as_ref()
        .is_some_and(|load_buffering| load_buffering.discarding)
}

/// Returns `true` if the execution goes on after a panic of the current
/// thread: the panic is held back, or the execution is being dropped.
pub(crate) fn carries_on(execution: &Execution) -> bool {
    execution
        .load_buffering
        .as_ref()
        .is_some_and(|load_buffering| {
            load_buffering.discarding || !load_buffering.pending.is_empty()
        })
}

/// Returns `true` if the current thread is to unwind now because the
/// execution is being dropped: it is running its body, where that is caught,
/// and is not unwinding already. Outside of its body, it is starting or
/// finishing, and gets to finish.
pub(crate) fn should_unwind(execution: &Execution) -> bool {
    is_discarding(execution)
        && !std::thread::panicking()
        && execution.threads.is_active()
        && execution.threads.active().in_body
}

/// Unwinds the current thread if the execution is being dropped. Called when
/// a thread is resumed.
pub(crate) fn unwind_if_discarding() {
    if Scheduler::try_with_execution(|execution| should_unwind(execution)).unwrap_or(false) {
        resume_unwind(Box::new(Discard));
    }
}

/// Sets whether the current thread runs its body.
fn set_in_body(in_body: bool) {
    Scheduler::with_execution(|execution| execution.threads.active_mut().in_body = in_body);
}

/// Runs the body of a model thread, the main one included.
///
/// A panic while a guess is unsettled is held back, and the thread ends
/// without it: the execution may not exist. While the execution is being
/// dropped, nothing a thread does counts.
pub(crate) fn run_thread(f: impl FnOnce()) {
    let enabled = Scheduler::with_execution(|execution| {
        if is_discarding(execution) {
            None
        } else {
            Some(execution.load_buffering.is_some())
        }
    });

    match enabled {
        // Dropped before it started. What the body owns is dropped, without
        // running it.
        None => return drop(f),
        Some(false) => return f(),
        Some(true) => {}
    }

    set_in_body(true);
    let result = catch_unwind(AssertUnwindSafe(f));
    set_in_body(false);

    let payload = match result {
        Ok(()) => return,
        Err(payload) => payload,
    };

    if payload.is::<Discard>() {
        return;
    }

    let held = Scheduler::with_execution(|execution| {
        let load_buffering = execution.load_buffering.as_mut().unwrap();

        load_buffering.discarding || !load_buffering.pending.is_empty()
    });

    if !held {
        resume_unwind(payload);
    }

    Scheduler::with_execution(|execution| {
        let load_buffering = execution.load_buffering.as_mut().unwrap();

        if !load_buffering.discarding && load_buffering.failure.is_none() {
            load_buffering.failure = Some(payload);
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    /// The usual exploration, which records the values stored to atomics.
    Collect,

    /// Exploration in which `Relaxed` loads may return guessed values.
    Guess,
}

pub(crate) struct LoadBuffering {
    pub(crate) pass: Pass,

    /// The values stored to each atomic, by its object index, in the
    /// executions of the first pass, in ascending order, with the threads
    /// (as a bit set) that stored them.
    pub(crate) values: BTreeMap<usize, Vec<(u64, ThreadSet)>>,

    /// The guesses of the current execution that are not settled yet.
    pub(crate) pending: Vec<Guess>,

    /// The guesses of the current execution that have been settled.
    pub(crate) settled: Vec<Settled>,

    /// Whether the current execution has guessed at all.
    pub(crate) guessed: bool,

    /// A panic held back until it is known whether the execution exists.
    pub(crate) failure: Option<Box<dyn Any + Send>>,

    /// Set when the current execution is being dropped.
    pub(crate) discarding: bool,
}

/// A load that returned a guessed value.
#[derive(Debug)]
pub(crate) struct Guess {
    /// The loading thread.
    pub(crate) thread: usize,

    /// The atomic's object index.
    pub(crate) atomic: usize,

    pub(crate) value: u64,

    /// What the thread knew of at the load.
    pub(crate) causality: VersionVec,

    /// The thread's own clock at the load. A store that happens after the
    /// load knows of it.
    pub(crate) clock: u16,

    /// The stores of the atomic that the thread knew of at the load, by
    /// their number among the stores to the atomic: the store that is read
    /// comes after them.
    pub(crate) known: Vec<u16>,

    /// The `SeqCst` fences before the load.
    pub(crate) fences: NodeSet,

    /// The number of stores of the atomic that had been forgotten at the
    /// load.
    pub(crate) forgotten: u16,

    /// The stores of the atomic, by number, that threads which knew of the
    /// load have read since. The store the load reads comes before them.
    pub(crate) read_after: Vec<u16>,
}

/// A guess that has been settled: the load, and the store it read.
#[derive(Debug)]
pub(crate) struct Settled {
    pub(crate) thread: usize,
    pub(crate) clock: u16,
    pub(crate) atomic: usize,

    /// The store's number among the stores to the atomic.
    pub(crate) seq: u16,
}

impl Guess {
    /// Returns `true` if the load knew of the operation of `thread` at its
    /// clock `clock`, that thread being another one.
    pub(crate) fn knows(&self, thread: usize, clock: u16) -> bool {
        thread != self.thread && self.causality.get(thread) >= clock
    }
}

impl LoadBuffering {
    pub(crate) fn new() -> LoadBuffering {
        LoadBuffering {
            pass: Pass::Collect,
            values: BTreeMap::new(),
            pending: Vec::new(),
            settled: Vec::new(),
            guessed: false,
            failure: None,
            discarding: false,
        }
    }

    /// Prepares for the next execution.
    pub(crate) fn reset(&mut self) {
        self.pending.clear();
        self.settled.clear();
        self.guessed = false;
        self.failure = None;
        self.discarding = false;
    }

    /// What the current execution comes to: `None` if it does not exist,
    /// otherwise the failure held back, if there is one.
    pub(crate) fn outcome(&mut self) -> Option<Option<Box<dyn Any + Send>>> {
        if self.discarding || !self.pending.is_empty() {
            None
        } else {
            Some(self.failure.take())
        }
    }

    /// Notes a store of `value` to `atomic` by `thread`, in the first pass.
    pub(crate) fn note_value(&mut self, atomic: usize, value: u64, thread: usize) {
        if self.pass == Pass::Collect {
            let values = self.values.entry(atomic).or_default();

            match values.binary_search_by_key(&value, |&(value, _)| value) {
                Ok(pos) => values[pos].1 |= 1 << thread,
                Err(pos) => values.insert(pos, (value, 1 << thread)),
            }
        }
    }

    /// Returns `true` if `thread` has an unsettled guess about `atomic`.
    pub(crate) fn guessing(&self, thread: usize, atomic: usize) -> bool {
        self.pending
            .iter()
            .any(|guess| guess.thread == thread && guess.atomic == atomic)
    }

    /// Returns `true` if `thread` has an unsettled guess about any atomic.
    pub(crate) fn thread_guessing(&self, thread: usize) -> bool {
        self.pending.iter().any(|guess| guess.thread == thread)
    }
}

impl fmt::Debug for LoadBuffering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadBuffering")
            .field("pass", &self.pass)
            .field("pending", &self.pending)
            .field("guessed", &self.guessed)
            .field("failure", &self.failure.is_some())
            .field("discarding", &self.discarding)
            .finish()
    }
}
