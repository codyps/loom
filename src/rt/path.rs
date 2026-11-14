use crate::rt::{execution, object, thread, MAX_ATOMIC_HISTORY, MAX_THREADS};

#[cfg(feature = "checkpoint")]
use serde::{Deserialize, Serialize};

/// An execution path
#[derive(Debug)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) struct Path {
    /// Present only when bytes select a single execution instead of exploration.
    #[cfg_attr(feature = "checkpoint", serde(skip))]
    input: Option<std::vec::IntoIter<u8>>,

    preemption_bound: Option<u8>,

    /// Current execution's position in the branches vec.
    ///
    /// When the execution starts, this is zero, but `branches` might not be
    /// empty.
    ///
    /// In order to perform an exhaustive search, the execution is seeded with a
    /// set of branches.
    pos: usize,

    /// List of all branches in the execution.
    ///
    /// A branch is of type `Schedule`, `Load`, or `Spurious`
    branches: object::Store<Entry>,

    /// If true, exploring is enabled at start
    exploring: bool,

    /// If true, the user decided to skip the current execution branch. We do
    /// not do any further exploration here.
    skipping: bool,

    /// How to reset the `exploring` state
    exploring_on_start: bool,

    /// If true, threads may be put to sleep, see `sleep_sets`.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    sleep_sets: bool,
}

#[derive(Debug)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) struct Schedule {
    /// Number of times the thread leading to this branch point has been
    /// pre-empted.
    preemptions: u8,

    /// The thread that was active first
    initial_active: Option<u8>,

    /// State of each thread
    threads: [Thread; MAX_THREADS],

    /// The previous schedule branch
    prev: Option<object::Ref<Schedule>>,

    /// The threads (as a bit set) that are asleep at this point: running
    /// them from here leads to nothing that has not been explored already,
    /// see `Execution::schedule`.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    sleeping: u8,

    /// The threads (as a bit set) that, when run from this point, could not
    /// carry on after their step: switching away from them at the next
    /// point was not a preemption.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    stopped: u8,

    exploring: bool,
}

#[derive(Debug)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) struct Load {
    /// All possible values
    values: [u8; MAX_ATOMIC_HISTORY],

    /// Current value
    pos: u8,

    /// Number of values in list
    len: u8,

    exploring: bool,
}

#[derive(Debug)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) struct Spurious {
    spur: bool,
    exploring: bool,
}

objects! {
    #[derive(Debug)]
    #[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
    Entry,
    Schedule(Schedule),
    Load(Load),
    Spurious(Spurious),
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) enum Thread {
    /// The thread is currently disabled
    Disabled,

    /// The thread should not be explored
    Skip,

    /// The thread is in a yield state.
    Yield,

    /// The thread is waiting to be explored
    Pending,

    /// The thread is currently being explored
    Active,

    /// The thread has been explored
    Visited,
}

macro_rules! assert_path_len {
    ($branches:expr) => {{
        assert!(
            // if we are panicking, we may be performing a branch due to a
            // `Drop` impl (e.g., for `Arc`, or for a user type that does an
            // atomic operation in its `Drop` impl). if that's the case,
            // asserting this again will double panic. therefore, short-circuit
            // the assertion if the thread is panicking.
            $branches.len() < $branches.capacity() || std::thread::panicking(),
            "Model exceeded maximum number of branches. This is often caused \
             by an algorithm requiring the processor to make progress, e.g. \
             spin locks.",
        );
    }};
}

impl Path {
    /// Create a new, blank, configured to branch at most `max_branches` times
    /// and at most `preemption_bound` thread preemptions.
    pub(crate) fn new(max_branches: usize, preemption_bound: Option<u8>, exploring: bool) -> Path {
        Path {
            input: None,
            preemption_bound,
            pos: 0,
            branches: object::Store::with_capacity(max_branches),
            exploring,
            skipping: false,
            exploring_on_start: exploring,
            sleep_sets: false,
        }
    }

    pub(crate) fn set_input(&mut self, input: &[u8]) {
        self.input = Some(Vec::from(input).into_iter());
    }

    fn input_choice(&mut self, count: usize) -> usize {
        if count <= 1 || !self.exploring {
            return 0;
        }
        self.input.as_mut().unwrap().next().unwrap_or(0) as usize % count
    }

    pub(crate) fn explore_state(&mut self) {
        if !self.skipping {
            assert!(!self.exploring, "not in critical state");
            self.exploring = true;
        }
    }

    pub(crate) fn critical(&mut self) {
        if !self.skipping {
            assert!(self.exploring, "not in exploring state");
            self.exploring = false;
        }
    }

    pub(crate) fn skip_branch(&mut self) {
        self.exploring = false;
        self.skipping = true;
    }

    pub(crate) fn set_max_branches(&mut self, max_branches: usize) {
        self.branches
            .reserve_exact(max_branches - self.branches.len());
    }

    /// Returns `true` if the execution has reached a point where the known path
    /// has been traversed and has reached a new branching point.
    pub(super) fn is_traversed(&self) -> bool {
        self.pos == self.branches.len()
    }

    pub(super) fn pos(&self) -> usize {
        self.pos
    }

    /// Push a new atomic-load branch
    pub(super) fn push_load(&mut self, seed: &[u8]) {
        assert_path_len!(self.branches);

        let pos = if self.input.is_some() {
            self.input_choice(seed.len()) as u8
        } else {
            0
        };

        let load_ref = self.branches.insert(Load {
            values: [0; MAX_ATOMIC_HISTORY],
            pos,
            len: 0,
            exploring: self.exploring,
        });

        let load = load_ref.get_mut(&mut self.branches);

        for (i, &store) in seed.iter().enumerate() {
            assert!(
                store < MAX_ATOMIC_HISTORY as u8,
                "[loom internal bug] store = {}; max = {}",
                store,
                MAX_ATOMIC_HISTORY
            );
            assert!(
                i < MAX_ATOMIC_HISTORY,
                "[loom internal bug] i = {}; max = {}",
                i,
                MAX_ATOMIC_HISTORY
            );

            load.values[i] = store;
            load.len += 1;
        }
    }

    /// Returns the atomic write to read
    pub(super) fn branch_load(&mut self) -> usize {
        assert!(!self.is_traversed(), "[loom internal bug]");

        let load = object::Ref::from_usize(self.pos, self.branches.id())
            .downcast::<Load>(&self.branches)
            .expect("Reached unexpected exploration state. Is the model fully deterministic?")
            .get(&self.branches);

        self.pos += 1;

        load.values[load.pos as usize] as usize
    }

    /// Branch on spurious notifications
    pub(super) fn branch_spurious(&mut self) -> bool {
        if self.is_traversed() {
            assert_path_len!(self.branches);

            let spur = self.input.is_some() && self.input_choice(2) != 0;

            self.branches.insert(Spurious {
                spur,
                exploring: self.exploring,
            });
        }

        let spurious = object::Ref::from_usize(self.pos, self.branches.id())
            .downcast::<Spurious>(&self.branches)
            .expect("Reached unexpected exploration state. Is the model fully deterministic?")
            .get(&self.branches)
            .spur;

        self.pos += 1;
        spurious
    }

    /// Returns the thread identifier to schedule
    /// Returns `true` if threads can be put to sleep. That relies on
    /// everything below a branch having been explored once the search has
    /// moved past it, which a single execution chosen by input makes
    /// meaningless. With a preemption bound, what has been explored is less,
    /// see `explored_at`.
    pub(super) fn sleep_sets(&self) -> bool {
        self.sleep_sets && self.input.is_none() && !self.skipping
    }

    pub(crate) fn set_sleep_sets(&mut self, sleep_sets: bool) {
        self.sleep_sets = sleep_sets;
    }

    /// Records that `thread`, run from the schedule branch at `pos`, could
    /// not carry on after its step.
    pub(super) fn note_stopped(&mut self, pos: usize, thread: usize) {
        if let Some(schedule) =
            object::Ref::from_usize(pos, self.branches.id()).downcast::<Schedule>(&self.branches)
        {
            schedule.get_mut(&mut self.branches).stopped |= 1 << thread;
        }
    }

    /// The threads (as a bit set) that have been explored from the schedule
    /// branch at `pos`, and that do not have to be run again further down the
    /// path that starts with `next` from there, until something happens that
    /// their next operation depends on.
    ///
    /// Without a preemption bound that is every thread that has been
    /// explored from there. With one, having explored a thread only says
    /// something about the executions that were within the bound. Take an
    /// execution that runs `next` and some more from here, and then gets to
    /// the thread's step. Moving that step up to this point gives the
    /// equivalent execution that has been explored, if it is within the
    /// bound too, which it is if it has no more preemptions than the first
    /// one. The two differ in three places:
    ///
    /// - Here: running the thread instead of `next`.
    /// - After the thread's step: switching to `next`, which preempts the
    ///   thread unless it cannot carry on.
    /// - At the thread's step in the first execution: the thread that ran
    ///   before it is preempted, or not, in favor of this thread in the one
    ///   and of whichever runs next in the other. In the worst case that
    ///   costs the first execution nothing more than the second.
    ///
    /// So the thread only sleeps if choosing it here, and leaving it after
    /// its step, was no more of a preemption than choosing `next` is.
    pub(super) fn explored_at(&self, pos: usize, next: usize) -> u8 {
        if pos >= self.branches.len() {
            return 0;
        }

        let schedule = match object::Ref::from_usize(pos, self.branches.id())
            .downcast::<Schedule>(&self.branches)
        {
            Some(schedule) => schedule.get(&self.branches),
            None => return 0,
        };

        let bounded = self.preemption_bound.is_some();

        // The cost of choosing a thread at this point.
        let preempts = |thread: usize| {
            schedule.initial_active.is_some() && schedule.initial_active != Some(thread as u8)
        };

        schedule
            .threads
            .iter()
            .enumerate()
            .filter(|(_, th)| **th == Thread::Visited)
            .filter(|&(i, _)| {
                if !bounded {
                    return true;
                }

                let carries_on = schedule.stopped & (1 << i) == 0;

                preempts(i) as u8 + carries_on as u8 <= preempts(next) as u8
            })
            .fold(0, |visited, (i, _)| visited | 1 << i)
    }

    pub(super) fn branch_thread(
        &mut self,
        execution_id: execution::Id,
        sleeping: u8,
        seed: impl ExactSizeIterator<Item = Thread>,
    ) -> Option<thread::Id> {
        if self.is_traversed() {
            assert_path_len!(self.branches);

            // Find the last thread scheduling branch in the path
            let prev = self.last_schedule();

            // Entering a new exploration space.
            //
            // Initialize a  new branch. The initial field values don't matter
            // as they will be updated below.
            let schedule_ref = self.branches.insert(Schedule {
                preemptions: 0,
                initial_active: None,
                threads: [Thread::Disabled; MAX_THREADS],
                prev,
                sleeping,
                stopped: 0,
                exploring: self.exploring,
            });

            // Get a reference to the branch in the object store.
            let schedule = schedule_ref.get_mut(&mut self.branches);

            assert!(seed.len() <= MAX_THREADS, "[loom internal bug]");

            // Currently active thread
            let mut active = None;

            for (i, v) in seed.enumerate() {
                // Initialize thread states
                schedule.threads[i] = v;

                if v.is_active() {
                    assert!(
                        active.is_none(),
                        "[loom internal bug] only one thread should start as active"
                    );
                    active = Some(i as u8);
                }
            }

            // Ensure at least one thread is active, otherwise toggle a yielded
            // thread.
            if active.is_none() {
                for (i, th) in schedule.threads.iter_mut().enumerate() {
                    if *th == Thread::Yield {
                        *th = Thread::Active;
                        active = Some(i as u8);
                        break;
                    }
                }
            }

            // The thread that is active to begin with is the one that ran up
            // to here, if it can carry on: choosing another one preempts it.
            // Otherwise a thread had to be picked, and no choice is a
            // preemption. Up to the first decision of an execution it is the
            // main thread that ran.
            let ran = match prev {
                Some(prev) => prev.get(&self.branches).active_thread_index(),
                None => Some(0),
            };

            let initial_active = if active == ran { active } else { None };

            let preemptions = if self.input.is_some() && self.preemption_bound.is_none() {
                // An unbounded input can force more preemptions than fit in u8.
                0
            } else {
                prev.map(|prev| prev.get(&self.branches).preemptions())
                    .unwrap_or(0)
            };

            debug_assert!(
                self.preemption_bound.is_none() || Some(preemptions) <= self.preemption_bound,
                "[loom internal bug] max = {:?}; curr = {}",
                self.preemption_bound,
                preemptions,
            );

            let schedule = schedule_ref.get_mut(&mut self.branches);
            schedule.initial_active = initial_active;
            schedule.preemptions = preemptions;

            if self.input.is_some() && self.exploring {
                self.select_thread_from_input(schedule_ref);
            }
        }

        let schedule = object::Ref::from_usize(self.pos, self.branches.id())
            .downcast::<Schedule>(&self.branches)
            .expect("Reached unexpected exploration state. Is the model fully deterministic?")
            .get(&self.branches);

        self.pos += 1;

        schedule
            .threads
            .iter()
            .enumerate()
            .find(|&(_, th)| th.is_active())
            .map(|(i, _)| thread::Id::new(execution_id, i))
    }

    fn select_thread_from_input(&mut self, schedule_ref: object::Ref<Schedule>) {
        let schedule = schedule_ref.get(&self.branches);
        let at_bound = self.preemption_bound == Some(schedule.preemptions)
            && schedule.initial_active.is_some();
        let mut choices = [0; MAX_THREADS];
        let mut count = 0;
        for (i, th) in schedule.threads.iter().enumerate() {
            if matches!(th, Thread::Active | Thread::Skip)
                && (!at_bound || schedule.initial_active == Some(i as u8))
            {
                choices[count] = i;
                count += 1;
            }
        }

        if count == 0 {
            return;
        }

        let selected = choices[self.input_choice(count)];
        let schedule = schedule_ref.get_mut(&mut self.branches);
        for th in &mut schedule.threads {
            if th.is_active() {
                *th = Thread::Skip;
            }
        }
        schedule.threads[selected] = Thread::Active;
    }

    /// Asks for `thread_id` to be tried from the scheduling decision at
    /// `point`, or the closest one before it. `last_step` is the decision
    /// the thread last ran from, if it has run at all.
    pub(super) fn backtrack(
        &mut self,
        mut point: usize,
        thread_id: thread::Id,
        last_step: Option<usize>,
    ) {
        // Whether the thread is, at the given decision, about to do what it
        // is about to do now.
        let same_step = |decision: usize| last_step.map_or(true, |last| last < decision);

        if self.input.is_some() {
            return;
        }
        let schedule = loop {
            if let Some(schedule_ref) = object::Ref::from_usize(point, self.branches.id())
                .downcast::<Schedule>(&self.branches)
            {
                let schedule = schedule_ref.get_mut(&mut self.branches);

                if schedule.exploring {
                    schedule.backtrack(thread_id, self.preemption_bound, same_step(point));
                    break schedule;
                }
            }

            if point == 0 {
                return;
            }

            point -= 1;
        };

        let mut curr = if let Some(curr) = schedule.prev {
            curr
        } else {
            return;
        };

        if self.preemption_bound.is_some() {
            loop {
                // Preemption bounded DPOR requires conservatively adding
                // another backtrack point to cover cases missed by the bounds.
                if let Some(prev) = curr.get(&self.branches).prev {
                    let active_a = curr.get(&self.branches).active_thread_index();
                    let active_b = prev.get(&self.branches).active_thread_index();

                    if active_a != active_b && curr.get_mut(&mut self.branches).exploring {
                        let same_step = same_step(curr.index());

                        curr.get_mut(&mut self.branches).backtrack(
                            thread_id,
                            self.preemption_bound,
                            same_step,
                        );
                        return;
                    }

                    curr = prev;
                } else {
                    if curr.get(&mut self.branches).exploring {
                        // This is the very first schedule
                        let same_step = same_step(curr.index());

                        curr.get_mut(&mut self.branches).backtrack(
                            thread_id,
                            self.preemption_bound,
                            same_step,
                        );
                    }
                    return;
                }
            }
        }
    }

    /// Reset the path to prepare for the next exploration of the model.
    ///
    /// This function will also trim the object store, dropping any objects that
    /// are created in pruned sections of the path.
    pub(super) fn step(&mut self) -> bool {
        // Reset the position to zero, the path will start traversing from the
        // beginning
        self.pos = 0;

        // Reset exploring / critical / skip
        self.exploring = self.exploring_on_start;
        self.skipping = false;

        // Set the final branch to try the next option. If all options have been
        // traversed, pop the final branch and try again w/ the one under it.
        //
        // This is depth-first tree traversal.
        //
        for last in (0..self.branches.len()).rev() {
            let last = object::Ref::from_usize(last, self.branches.id());

            // Remove all objects that were created **after** this branch
            self.branches.truncate(last);

            if let Some(schedule_ref) = last.downcast::<Schedule>(&self.branches) {
                let schedule = schedule_ref.get_mut(&mut self.branches);

                if !schedule.exploring {
                    continue;
                }

                // Transition the active thread to visited.
                if let Some(thread) = schedule.threads.iter_mut().find(|th| th.is_active()) {
                    *thread = Thread::Visited;
                }

                // Find a pending thread and transition it to active
                let rem = schedule
                    .threads
                    .iter_mut()
                    .find(|th| th.is_pending())
                    .map(|th| {
                        *th = Thread::Active;
                    })
                    .is_some();

                if rem {
                    return true;
                }
            } else if let Some(load_ref) = last.downcast::<Load>(&self.branches) {
                let load = load_ref.get_mut(&mut self.branches);

                if !load.exploring {
                    continue;
                }

                load.pos += 1;

                if load.pos < load.len {
                    return true;
                }
            } else if let Some(spurious_ref) = last.downcast::<Spurious>(&self.branches) {
                let spurious = spurious_ref.get_mut(&mut self.branches);

                if !spurious.exploring {
                    continue;
                }

                if !spurious.spur {
                    spurious.spur = true;
                    return true;
                }
            } else {
                unreachable!();
            }
        }

        false
    }

    fn last_schedule(&self) -> Option<object::Ref<Schedule>> {
        self.branches.iter_ref::<Schedule>().rev().next()
    }
}

impl Schedule {
    /// Returns the index of the currently active thread
    fn active_thread_index(&self) -> Option<u8> {
        self.threads
            .iter()
            .enumerate()
            .find(|(_, th)| th.is_active())
            .map(|(index, _)| index as u8)
    }

    /// Compute the number of preemptions for the current state of the branch
    fn preemptions(&self) -> u8 {
        if self.initial_active.is_some() && self.initial_active != self.active_thread_index() {
            return self.preemptions + 1;
        }

        self.preemptions
    }

    /// `same_step` says that the thread has not run since this decision:
    /// what it would do from here is the step that is wanted earlier.
    fn backtrack(&mut self, thread_id: thread::Id, preemption_bound: Option<u8>, same_step: bool) {
        assert!(self.exploring);

        if let Some(bound) = preemption_bound {
            assert!(
                self.preemptions <= bound,
                "[loom internal bug] actual = {}, bound = {}",
                self.preemptions,
                bound
            );

            // Out of preemptions, and choosing another thread here would
            // be one. It is not if the thread that ran up to here cannot
            // carry on: then some thread has to be chosen, and any of them
            // is as good as the next.
            if self.preemptions == bound && self.initial_active.is_some() {
                return;
            }
        }

        let thread_id = thread_id.as_usize();

        if thread_id >= self.threads.len() {
            return;
        }

        // The thread is to be tried from here because something it does
        // further down should come before something another thread does
        // there. If it can run here and is awake, that is what to try.
        //
        // If it is asleep, running it from here has been explored from an
        // earlier point, with nothing having happened since that would make
        // its next step turn out differently. That does not settle the
        // matter, though: what should come earlier may be a later step of
        // the thread, one that some other thread has to enable or to change
        // the outcome of first. Which thread that is, is not known here, so
        // as when the thread cannot run at all, every thread that is awake
        // gets tried.
        let asleep = self.sleeping & (1 << thread_id) != 0;

        if asleep && same_step {
            // The step that is wanted here is the thread's next one, and
            // the thread being asleep says that running it from here has
            // been explored. There is nothing to add.
            return;
        }

        if self.threads[thread_id].is_enabled() && !asleep {
            self.threads[thread_id].explore();
        } else {
            for (i, th) in self.threads.iter_mut().enumerate() {
                if self.sleeping & (1 << i) == 0 {
                    th.explore();
                }
            }
        }
    }
}

impl Thread {
    fn explore(&mut self) {
        if *self == Thread::Skip {
            *self = Thread::Pending;
        }
    }

    fn is_pending(&self) -> bool {
        *self == Thread::Pending
    }

    fn is_active(&self) -> bool {
        *self == Thread::Active
    }

    fn is_enabled(&self) -> bool {
        !self.is_disabled()
    }

    fn is_disabled(&self) -> bool {
        *self == Thread::Disabled
    }
}

#[cfg(test)]
mod tests {
    use super::{Path, Thread};
    use crate::rt::execution;

    #[test]
    fn choices_share_input_and_skip_single_outcomes() {
        let mut path = Path::new(16, None, true);
        path.set_input(&[5, 3]);
        let id = execution::Id::new();
        assert_eq!(
            path.branch_thread(id, 0, [Thread::Active].into_iter())
                .unwrap()
                .as_usize(),
            0
        );
        assert_eq!(
            path.branch_thread(
                id,
                0,
                [Thread::Active, Thread::Skip, Thread::Skip].into_iter()
            )
            .unwrap()
            .as_usize(),
            2
        );
        path.push_load(&[4]);
        assert_eq!(path.branch_load(), 4);
        path.push_load(&[2, 5]);
        assert_eq!(path.branch_load(), 5);
        assert!(!path.branch_spurious());
    }

    #[test]
    fn input_selects_spurious_outcome() {
        let mut path = Path::new(8, None, true);
        path.set_input(&[255, 2]);
        assert!(path.branch_spurious());
        assert!(!path.branch_spurious());
        assert!(!path.branch_spurious());
    }

    #[test]
    fn critical_section_preserves_input() {
        let mut path = Path::new(8, None, false);
        path.set_input(&[1]);
        assert!(!path.branch_spurious());
        path.explore_state();
        assert!(path.branch_spurious());
    }

    #[test]
    fn preemption_bound_keeps_active_thread_without_consuming_input() {
        let mut path = Path::new(16, Some(1), true);
        path.set_input(&[1, 0, 1]);
        let id = execution::Id::new();
        let next = |path: &mut Path, threads: [Thread; 2]| {
            path.branch_thread(id, 0, threads.into_iter())
                .unwrap()
                .as_usize()
        };

        assert_eq!(next(&mut path, [Thread::Active, Thread::Skip]), 1);
        assert_eq!(next(&mut path, [Thread::Skip, Thread::Active]), 1);
        assert_eq!(next(&mut path, [Thread::Active, Thread::Disabled]), 0);
        assert!(!path.branch_spurious());
        assert!(path.branch_spurious());
    }

    #[test]
    fn yielded_threads_wait_for_a_runnable_thread() {
        let mut path = Path::new(8, None, true);
        path.set_input(&[1]);
        let id = execution::Id::new();
        assert_eq!(
            path.branch_thread(id, 0, [Thread::Yield, Thread::Active].into_iter())
                .unwrap()
                .as_usize(),
            1
        );
        assert!(path.branch_spurious());
    }
}
