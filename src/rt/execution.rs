use crate::rt::alloc::Allocation;
use crate::rt::seq_cst::SeqCst;
use crate::rt::speculation::{self, LoadBuffering};
use crate::rt::{lazy_static, object, thread, Path, ThreadSet, VersionVec, MAX_THREADS};

use std::collections::HashMap;
use std::fmt;

use tracing::info;

pub(crate) struct Execution {
    /// Uniquely identifies an execution
    pub(super) id: Id,

    /// Execution path taken
    pub(crate) path: Path,

    pub(crate) threads: thread::Set,

    pub(crate) lazy_statics: lazy_static::Set,

    /// All loom aware objects part of this execution run.
    pub(super) objects: object::Store,

    /// The order of `SeqCst` operations.
    pub(super) seq_cst: SeqCst,

    /// The threads (as a bit set) that are asleep: each is about to execute
    /// an operation that has already been explored from an earlier point of
    /// this execution, with nothing dependent on it having happened since.
    /// Scheduling such a thread now can only lead to executions that are
    /// equivalent to ones explored from that earlier point.
    sleeping: ThreadSet,

    /// The position in the path of the scheduling decision that the active
    /// thread is running from.
    scheduled_at: Option<usize>,

    /// The steps of this execution so far, in order.
    steps: Vec<Step>,

    /// The pairs of operations of this execution that could have been the
    /// other way around: the position in the path of the scheduling
    /// decision that the earlier one was executed from, and the step of the
    /// later one.
    races: Vec<(usize, usize)>,

    /// Maps raw allocations to LeakTrack objects
    pub(super) raw_allocations: HashMap<usize, Allocation>,

    pub(crate) arc_objs: HashMap<*const (), std::sync::Arc<super::Arc>>,

    /// Maximum number of concurrent threads
    pub(super) max_threads: usize,

    /// The number of stores kept per atomic.
    pub(crate) max_history: usize,

    /// Capture locations for significant events
    pub(crate) location: bool,

    /// Log execution output to STDOUT
    pub(crate) log: bool,

    /// Whether a condition variable wait may end without a notification.
    pub(crate) spurious_wakeups: bool,

    /// Guesses for loads that read stores yet to be made, when load
    /// buffering is explored (see `rt::speculation`).
    pub(crate) load_buffering: Option<Box<LoadBuffering>>,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub(crate) struct Id(usize);

/// A step of an execution: a thread, chosen at a scheduling decision, runs
/// its operation.
struct Step {
    /// The position in the path of the scheduling decision.
    path_id: usize,

    thread: usize,

    /// What the step is ordered after, itself included.
    dpor_vv: VersionVec,

    /// Whether the step was an operation on something, and not just a
    /// thread getting to its first one.
    operation: bool,
}

impl Execution {
    /// Create a new execution.
    ///
    /// This is only called at the start of a fuzz run. The same instance is
    /// reused across permutations.
    pub(crate) fn new(
        max_threads: usize,
        max_branches: usize,
        preemption_bound: Option<usize>,
        exploring: bool,
    ) -> Execution {
        let id = Id::new();
        let threads = thread::Set::new(id, max_threads);

        let preemption_bound =
            preemption_bound.map(|bound| bound.try_into().expect("preemption_bound too big"));

        Execution {
            id,
            path: Path::new(max_branches, preemption_bound, exploring),
            threads,
            lazy_statics: lazy_static::Set::new(),
            objects: object::Store::with_capacity(max_branches),
            seq_cst: SeqCst::default(),
            sleeping: 0,
            scheduled_at: None,
            steps: Vec::new(),
            races: Vec::new(),
            raw_allocations: HashMap::new(),
            arc_objs: HashMap::new(),
            max_threads,
            max_history: 7,
            location: false,
            log: false,
            spurious_wakeups: true,
            load_buffering: None,
        }
    }

    /// Create state to track a new thread
    pub(crate) fn new_thread(&mut self) -> thread::Id {
        let thread_id = self.threads.new_thread();
        let active_id = self.threads.active_id();

        let (active, new) = self.threads.active2_mut(thread_id);

        new.causality.join(&active.causality);
        new.dpor_vv.join(&active.dpor_vv);

        // Bump causality in order to ensure CausalCell accurately detects
        // incorrect access when first action.
        new.causality[thread_id] += 1;
        active.causality[active_id] += 1;

        thread_id
    }

    /// Resets the execution state for the next execution run
    pub(crate) fn step(self) -> Option<Self> {
        let id = Id::new();
        let max_threads = self.max_threads;
        let max_history = self.max_history;
        let location = self.location;
        let log = self.log;
        let spurious_wakeups = self.spurious_wakeups;
        let mut load_buffering = self.load_buffering;
        let mut path = self.path;
        let mut objects = self.objects;
        let mut seq_cst = self.seq_cst;
        let mut lazy_statics = self.lazy_statics;
        let mut raw_allocations = self.raw_allocations;
        let mut arc_objs = self.arc_objs;
        let mut steps = self.steps;
        let mut races = self.races;

        let mut threads = self.threads;

        if path.optimal() {
            wake_up(&mut path, &steps, &races, self.id);
        }

        races.clear();

        if !path.step() {
            return None;
        }

        objects.clear();
        seq_cst.clear();
        lazy_statics.reset();
        raw_allocations.clear();
        arc_objs.clear();
        steps.clear();

        threads.clear(id);

        if let Some(load_buffering) = &mut load_buffering {
            load_buffering.reset();
        }

        Some(Execution {
            id,
            path,
            threads,
            objects,
            seq_cst,
            sleeping: 0,
            scheduled_at: None,
            steps,
            races,
            lazy_statics,
            raw_allocations,
            arc_objs,
            max_threads,
            max_history,
            location,
            log,
            spurious_wakeups,
            load_buffering,
        })
    }

    /// Scheduling while the execution is being dropped (see
    /// `speculation::Discard`): the current thread unwinds, unless it is
    /// unwinding already, and when it is done, the next thread that has not
    /// finished runs to unwind in turn, blocked or not. Nothing is recorded
    /// in the path.
    fn schedule_discarding(&mut self) -> bool {
        let current = self.threads.active_id();

        if !self.threads.active().is_terminated() {
            if speculation::should_unwind(self) {
                std::panic::resume_unwind(Box::new(speculation::Discard));
            }

            // Unwinding already, or starting or finishing: carry on.
            return false;
        }

        let next = self
            .threads
            .iter()
            .map(|(id, _)| id)
            .find(|&id| !self.threads[id].is_terminated());

        if let Some(next) = next {
            self.threads[next].set_runnable();
        }

        self.threads.set_active(next);

        next.is_some() && next != Some(current)
    }

    /// Returns `true` if a switch is required
    pub(crate) fn schedule(&mut self) -> bool {
        use crate::rt::path::Thread;

        // Implementation of the DPOR algorithm.

        if speculation::is_discarding(self) {
            return self.schedule_discarding();
        }

        let curr_thread = self.threads.active_id();

        self.release_yielded();
        self.backtrack_yield();

        if let Some(pos) = self.scheduled_at {
            if self.threads.active().is_runnable() {
                self.path.note_carried_on(pos, curr_thread.as_usize());
            }
        }

        // With wakeup trees, what could have been the other way around is
        // looked at when an operation is executed, further down, and acted
        // on when the execution has come to its end.
        let optimal = self.path.optimal();

        for (th_id, th) in self.threads.iter() {
            if optimal {
                break;
            }

            let operation = match th.operation {
                Some(operation) => operation,
                None => continue,
            };

            // Find the dependent accesses that are not ordered before this
            // operation: the two race, and the other order has to be
            // explored as well. Usually it is enough to do that for the most
            // recent one, see `races_with_all`.
            let all = self.objects.races_with_all(operation);
            let mut latest = None;
            let mut points = Vec::new();
            let path = &mut self.path;

            // Taking a lock cannot be moved before the release it waits
            // for, and without a bound nothing is gained by trying. With a
            // bound it is: the thread can be run up to where it starts to
            // wait, at a point where the other one still holds the lock,
            // and switching away from a thread that waits is no
            // preemption. An execution that is within the bound that way
            // may have no other within it that is equivalent.
            let bounded = path.is_bounded();

            self.objects
                .for_each_dependent_access(operation, true, bounded, |access| {
                    if access.happens_before(&th.dpor_vv) {
                        // The previous access happened before this access,
                        // thus there is no race.
                        return;
                    }

                    if all {
                        points.push(access.path_id());
                    } else {
                        latest = latest.max(Some(access.path_id()));
                    }
                });

            points.extend(latest);

            if points.is_empty() {
                continue;
            }

            // What the operation is ordered after.
            let mut dpor_vv = th.dpor_vv;

            self.objects
                .for_each_dependent_access(operation, false, false, |access| {
                    dpor_vv.join(access.version());
                });

            for point in points {
                if !bounded && covered(&self.steps, path, point, th_id.as_usize(), &dpor_vv) {
                    continue;
                }

                // Track backtracking point
                path.backtrack(point, th_id);
            }
        }

        // It's important to avoid pre-emption as much as possible
        let mut initial = Some(self.threads.active_id());

        if !self.path.sleep_sets() {
            self.sleeping = 0;
        }

        // If the thread is not runnable, then we can pick any arbitrary other
        // runnable thread.
        if !self.threads.active().is_runnable() {
            initial = None;

            // A thread that is awake, if there is one.
            for asleep in [false, true] {
                for (i, th) in self.threads.iter() {
                    if !th.is_runnable() {
                        continue;
                    }

                    if (self.sleeping & (1 << i.as_usize()) != 0) != asleep {
                        continue;
                    }

                    if let Some(ref mut init) = initial {
                        if th.yield_count < self.threads[*init].yield_count {
                            *init = i;
                        }
                    } else {
                        initial = Some(i)
                    }
                }

                if initial.is_some() {
                    if asleep && self.path.is_traversed() {
                        // Everything that can run is asleep, so this
                        // execution is going to repeat one that has been
                        // explored. It still has to run to its end, but
                        // there is nothing to be explored on the way.
                        self.path.skip_branch();
                        self.sleeping = 0;
                    }

                    break;
                }
            }
        }

        let path_id = self.path.pos();
        let sleeping = self.sleeping;

        let next = self.path.branch_thread(self.id, sleeping, {
            self.threads.iter().map(|(i, th)| {
                if initial.is_none() && th.is_runnable() {
                    initial = Some(i);
                }

                if initial == Some(i) {
                    Thread::Active
                } else if th.is_yield() {
                    Thread::Yield
                } else if !th.is_runnable() {
                    Thread::Disabled
                } else {
                    Thread::Skip
                }
            })
        });
        if let Some(next) = next {
            assert!(
                self.threads[next].is_runnable() || self.threads[next].is_yield(),
                "Picked a thread for execution that is not actually runnable",
            );
        }

        if next.is_none() && !self.threads.iter().all(|(_, th)| th.is_terminated()) {
            // No thread can run, and not because they are all done: the test
            // has deadlocked.
            //
            // With a guess unsettled, nothing can settle it anymore: the
            // execution does not exist. With a failure held back while there
            // was one, the failure is what this comes from.
            if let Some(load_buffering) = &mut self.load_buffering {
                if !load_buffering.pending.is_empty() {
                    load_buffering.discarding = true;
                    return self.schedule_discarding();
                }

                if let Some(failure) = load_buffering.failure.take() {
                    std::panic::resume_unwind(failure);
                }
            }

            //
            // The current thread is left active. The panic below unwinds it,
            // and the destructors that run on the way (a loom `Arc` being
            // dropped, say) are loom operations that need an active thread:
            // without one they would panic again and abort the process,
            // hiding the deadlock report.
            assert!(
                std::thread::panicking(),
                "deadlock; threads = {:?}",
                self.threads
                    .iter()
                    .map(|(i, th)| { (i, th.state) })
                    .collect::<Vec<_>>()
            );

            // Already unwinding, with nothing else left to run: let the
            // current thread carry on with its destructors.
            return false;
        }

        let switched = Some(self.threads.active_id()) != next;

        // The threads that have been run from this point before are asleep
        // from here on, along with the ones that already were. They wake up
        // when something happens that their next operation depends on, which
        // may be the very operation that is about to be executed.
        if self.path.sleep_sets() {
            let mut sleeping = 0;

            if let Some(next) = next {
                let visited = self.path.explored_at(path_id, next.as_usize());

                let executed = self.threads[next].operation;

                for (i, th) in self.threads.iter() {
                    let bit = 1 << i.as_usize();

                    if i == next || (self.sleeping | visited) & bit == 0 {
                        continue;
                    }

                    let independent = match (th.operation, executed) {
                        (Some(pending), Some(executed)) => {
                            !self.objects.dependent(pending, executed)
                        }
                        // A thread without a pending operation is not at an
                        // operation at all: it is starting, or continuing
                        // after having been woken, and its step ends at the
                        // next operation it gets to, before executing it.
                        // Such a step touches nothing loom models, so it
                        // commutes with everything.
                        _ => true,
                    };

                    // With a preemption bound, moving a thread's step to an
                    // earlier point must not cost a preemption more (see
                    // `Path::explored_at`). That is only known for a step
                    // that leaves alone which threads can run: with a lock
                    // taken earlier, a thread that waits for it stops for
                    // free where it would have had to be preempted, and the
                    // other way around.
                    let bounded_out = self.path.is_bounded()
                        && th
                            .operation
                            .map_or(false, |pending| self.objects.affects_waiting(pending));

                    // A thread that has yielded can run again because of
                    // what other threads did since, so it does not have
                    // nothing to do with them.
                    if independent && th.is_runnable() && !th.yielded && !bounded_out {
                        sleeping |= bit;
                    }
                }
            }

            self.sleeping = sleeping;
        } else {
            self.sleeping = 0;
        }

        self.threads.set_active(next);
        self.scheduled_at = Some(path_id);

        if let Some(next) = next {
            self.threads[next].last_step = Some(path_id);
            self.threads[next].yielded = false;
        }

        // There is no active thread: all threads have terminated.
        if !self.threads.is_active() {
            return true;
        }

        // TODO: refactor
        if let Some(operation) = self.threads.active().operation {
            let threads = &mut self.threads;
            let th_id = threads.active_id();

            let mut dpor_vv = threads.active().dpor_vv;

            if optimal {
                // The accesses that the operation is dependent with and not
                // ordered after.
                let mut racing: Vec<(usize, VersionVec)> = Vec::new();

                self.objects
                    .for_each_dependent_access(operation, true, false, |access| {
                        if !access.happens_before(&dpor_vv) {
                            racing.push((access.path_id(), *access.version()));
                        }
                    });

                // Only the ones with nothing in between could have been the
                // other way around as they are.
                for (i, (path_id, version)) in racing.iter().enumerate() {
                    let direct = racing
                        .iter()
                        .enumerate()
                        .all(|(j, (_, later))| i == j || !(version <= later));

                    if direct {
                        self.races.push((*path_id, self.steps.len()));
                    }
                }
            }

            self.objects
                .for_each_dependent_access(operation, false, false, |access| {
                    dpor_vv.join(access.version());
                });

            dpor_vv[th_id] += 1;
            threads.active_mut().dpor_vv = dpor_vv;
        }

        self.steps.push(Step {
            path_id,
            thread: self.threads.active_id().as_usize(),
            dpor_vv: self.threads.active().dpor_vv,
            operation: self.threads.active().operation.is_some(),
        });

        if let Some(operation) = self.threads.active().operation {
            let th_id = self.threads.active_id();
            let dpor_vv = self.threads.active().dpor_vv;

            self.objects
                .set_last_access(operation, th_id.as_usize(), path_id, &dpor_vv);
        }

        // Reactivate yielded threads that have nothing to wait for, see
        // `release_yielded`, but not the one that is about to run.
        for (id, th) in self.threads.iter_mut() {
            if th.is_yield() && Some(id) != next && th.yield_waits == 0 {
                th.set_runnable();
            }
        }

        if switched {
            info!("~~~~~~~~ THREAD {} ~~~~~~~~", self.threads.active_id());
        }

        curr_thread != self.threads.active_id()
    }

    /// When the active thread has just yielded: asks for it to be tried
    /// before the last step of each other thread.
    ///
    /// A thread that yields runs again once the others have taken a step
    /// (see `release_yielded`), so where it yields relative to their steps
    /// decides what it gets to do before their next ones. No object says
    /// so: the yield and the step may have nothing else in common.
    fn backtrack_yield(&mut self) {
        let active = self.threads.active_id();

        if !self.threads.active().is_yield() {
            return;
        }

        let yielding = self.threads.active();
        let dpor_vv = yielding.dpor_vv;

        // The threads it waits for are not asleep anymore: their next step
        // lets this thread run again, which it did not when it was explored.
        self.sleeping &= !yielding.yield_waits;

        for (id, th) in self.threads.iter() {
            if id == active {
                continue;
            }

            let point = match th.last_step {
                Some(point) => point,
                None => continue,
            };

            // Not if the yielding thread could not have come first.
            if th.dpor_vv <= dpor_vv {
                continue;
            }

            self.path.backtrack(point, active);
        }
    }

    /// Decides which of the threads that have yielded can run again.
    ///
    /// A thread that yields says that it cannot get anywhere until some
    /// other thread does something. Which thread that is, is not known, so
    /// it waits for every thread that can run to take a step, or to become
    /// unable to. Without that, two threads that spin on a lock can hand the
    /// processor back and forth forever while the thread that holds the
    /// lock, and could release it, is never run: the search would follow
    /// them ever deeper.
    ///
    /// This is called when the active thread has taken a step.
    fn release_yielded(&mut self) {
        let active = self.threads.active_id();

        let runnable = self
            .threads
            .iter()
            .filter(|(_, th)| th.is_runnable())
            .fold(0 as ThreadSet, |set, (id, _)| set | 1 << id.as_usize());
        let others = runnable & !(1 << active.as_usize());

        for (_, th) in self.threads.iter_mut() {
            th.yield_waits &= others;
        }

        if self.threads.active().is_yield() {
            self.threads.active_mut().yield_waits = others;
        }

        // The threads that have nothing left to wait for can be chosen from
        // here on.
        for (id, th) in self.threads.iter_mut() {
            if id != active && th.is_yield() && th.yield_waits == 0 {
                th.set_runnable();
            }
        }
    }

    /// Panics if any leaks were detected
    pub(crate) fn check_for_leaks(&self) {
        self.objects.check_for_leaks();
    }
}

/// Goes through the operations of an execution that has come to its end
/// which could have been the other way around, and adds to the wakeup trees
/// what gets them that way.
///
/// For an operation and a later one that it races with, that is: from the
/// decision that the earlier one was executed from, the steps that were
/// taken after it and are not ordered after it, in the order in which they
/// were taken, and then the later operation.
fn wake_up(path: &mut Path, steps: &[Step], races: &[(usize, usize)], execution: Id) {
    use crate::rt::path::Wakeup;

    // Whether step `a` has to come before step `b`, which was taken later.
    let before = |a: usize, b: usize| {
        let (a, b) = (&steps[a], &steps[b]);

        a.thread == b.thread || (a.operation && b.dpor_vv.get(a.thread) >= a.dpor_vv.get(a.thread))
    };

    for &(point, later) in races {
        let earlier = match steps.binary_search_by_key(&point, |step| step.path_id) {
            Ok(earlier) => earlier,
            Err(_) => continue,
        };

        let mut sequence: Vec<Wakeup> = (earlier + 1..steps.len())
            .filter(|&step| step != later && !before(earlier, step))
            .map(|step| Wakeup {
                thread: steps[step].thread,
                step,
            })
            .collect();

        sequence.push(Wakeup {
            thread: steps[later].thread,
            step: later,
        });

        if !path.insert_wakeup(point, &sequence, before) {
            // No decision to add a sequence to: a single thread it is.
            path.backtrack(point, thread::Id::new(execution, steps[later].thread));
        }
    }
}

/// Returns `true` if nothing has to be added to the scheduling decision at
/// `point` for `thread`'s next operation, which is ordered after `dpor_vv`,
/// to be tried before the operation that was executed from there.
///
/// The executions wanted are the ones that go on, from that point, with
/// the steps that have been taken since and are not ordered after that
/// operation, and then with `thread`'s operation. Any thread whose first
/// step among those is not ordered after another of them can be the one to
/// start with. If one of those threads has been explored from the point, is
/// being explored, or is going to be, these executions are taken care of.
/// (These are the "source sets" of Abdulla, Aronis, Jonsson and Sagonas,
/// "Optimal dynamic partial order reduction", POPL 2014.)
fn covered(steps: &[Step], path: &Path, point: usize, thread: usize, dpor_vv: &VersionVec) -> bool {
    let index = match steps.binary_search_by_key(&point, |step| step.path_id) {
        Ok(index) => index,
        Err(_) => return false,
    };

    let earlier = &steps[index];
    let clock = earlier.dpor_vv.get(earlier.thread);

    // Each thread's first step that is not ordered after the operation.
    let mut first = [None; MAX_THREADS];

    for (i, step) in steps.iter().enumerate().skip(index + 1) {
        if step.dpor_vv.get(earlier.thread) < clock && first[step.thread].is_none() {
            first[step.thread] = Some(i);
        }
    }

    // Whether a step at position `at`, ordered after `vv`, comes after none
    // of the other threads' steps.
    let leads = |thread: usize, vv: &VersionVec, at: usize| {
        first.iter().enumerate().all(|(other, first)| match first {
            Some(first) if other != thread && *first < at => {
                vv.get(other) < steps[*first].dpor_vv.get(other)
            }
            _ => true,
        })
    };

    let mut initials: ThreadSet = 0;

    for (other, first) in first.iter().enumerate() {
        if let Some(first) = first {
            if leads(other, &steps[*first].dpor_vv, *first) {
                initials |= 1 << other;
            }
        }
    }

    if first[thread].is_none() && leads(thread, dpor_vv, usize::MAX) {
        initials |= 1 << thread;
    }

    path.is_scheduled(point, initials)
}

impl fmt::Debug for Execution {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Execution")
            .field("path", &self.path)
            .field("threads", &self.threads)
            .finish()
    }
}

impl Id {
    pub(crate) fn new() -> Id {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering::Relaxed;

        // The number picked here is arbitrary. It is mostly to avoid collision
        // with "zero" to aid with debugging.
        static NEXT_ID: AtomicUsize = AtomicUsize::new(46_413_762);

        let next = NEXT_ID.fetch_add(1, Relaxed);

        Id(next)
    }
}
