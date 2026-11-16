use crate::rt::{execution, object, thread, ThreadSet, MAX_ATOMIC_HISTORY, MAX_THREADS};

#[cfg(feature = "checkpoint")]
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

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

    /// If true, every thread that can run is tried at every scheduling
    /// decision, as far as the preemption bound allows, whether or not
    /// anything suggests that it makes a difference.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    exhaustive: bool,

    /// If true, wakeup trees are used where they can be, see `optimal`.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    wakeup_trees: bool,

    /// The sequences that the execution is to follow from the next branch
    /// that is created.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    guide: Wut,

    /// What this path has in common with the paths of the other workers of
    /// a search that is run by several, see `Shared`.
    #[cfg_attr(feature = "checkpoint", serde(skip))]
    shared: Option<Arc<Shared>>,
}

/// A wakeup tree: the sequences of threads that are still to be run from a
/// scheduling decision, as a tree in which the children of a node are in the
/// order in which they were added. A sequence ends where the search is free
/// to go on as it likes.
///
/// This is what makes the search explore each way of ordering the dependent
/// operations of a model once, and no execution in vain ("Optimal dynamic
/// partial order reduction", Abdulla, Aronis, Jonsson and Sagonas, POPL
/// 2014). When two operations of an execution could have been the other way
/// around, what gets it that way is not one thread to be run earlier but a
/// sequence: the steps that were taken in between and have nothing to do
/// with the first operation, and then the second one. Without the sequence,
/// the search starts the thread and may end up, many steps later, where it
/// has been.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) struct Wut {
    children: Vec<(u8, Wut)>,
}

/// A step in a sequence that is to be added to a wakeup tree.
#[derive(Debug, Clone, Copy)]
pub(super) struct Wakeup {
    pub(super) thread: usize,

    /// Identifies the step to the caller, see `Path::insert_wakeup`.
    pub(super) step: usize,
}

impl Wut {
    fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// The tree below the child for `thread`.
    fn subtree(&self, thread: usize) -> Wut {
        self.children
            .iter()
            .find(|(child, _)| *child as usize == thread)
            .map(|(_, subtree)| subtree.clone())
            .unwrap_or_default()
    }

    fn remove(&mut self, thread: usize) {
        self.children.retain(|(child, _)| *child as usize != thread);
    }

    /// A tree that is the single sequence `steps`.
    fn sequence(steps: &[Wakeup]) -> Wut {
        let mut tree = Wut::default();

        for step in steps.iter().rev() {
            tree = Wut {
                children: vec![(step.thread as u8, tree)],
            };
        }

        tree
    }
}

/// Identifies a branch, and whether other workers have a copy of it.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
struct Node {
    /// Unique among the branches of a search.
    id: u64,

    /// Set once the branch has been handed to another worker as part of the
    /// path that leads to its work. From then on an alternative of the
    /// branch is only taken by the worker that claims it first.
    shared: bool,
}

impl Node {
    fn new() -> Node {
        use std::sync::atomic::AtomicU64;

        static NEXT: AtomicU64 = AtomicU64::new(0);

        Node {
            id: NEXT.fetch_add(1, Relaxed),
            shared: false,
        }
    }

    /// Asks for the alternative `alt` of the branch. `Ok` if this worker
    /// gets to explore it, `Err` if another one has taken it; either way
    /// with the alternatives (as a bit set) that were taken before.
    fn claim(&self, shared: &Option<Arc<Shared>>, alt: u8) -> Result<ThreadSet, ThreadSet> {
        match shared {
            Some(shared) if self.shared => shared.claim(self.id, alt),
            _ => Ok(0),
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) struct Schedule {
    #[cfg_attr(feature = "checkpoint", serde(default))]
    node: Node,

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
    sleeping: ThreadSet,

    /// The sequences that are still to be run from here.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    wut: Wut,

    /// The threads (as a bit set) that, when run from this point, could
    /// carry on after their step: switching away from them at the next
    /// point was a preemption. A step can include choices of its own (which
    /// store a load returns, whether a wakeup is spurious), and end
    /// differently depending on them: this holds every thread that carried
    /// on for some of those.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    carried_on: ThreadSet,

    exploring: bool,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) struct Load {
    #[cfg_attr(feature = "checkpoint", serde(default))]
    node: Node,

    /// All possible values
    values: [u8; MAX_ATOMIC_HISTORY],

    /// Current value
    pos: u8,

    /// Number of values in list
    len: u8,

    /// What the execution is to follow after this branch, whichever value
    /// is read.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    guide: Wut,

    exploring: bool,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "checkpoint", derive(Serialize, Deserialize))]
pub(crate) struct Spurious {
    #[cfg_attr(feature = "checkpoint", serde(default))]
    node: Node,

    spur: bool,

    /// What the execution is to follow after this branch, either way.
    #[cfg_attr(feature = "checkpoint", serde(default))]
    guide: Wut,

    exploring: bool,
}

objects! {
    #[derive(Debug, Clone)]
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

    /// Another worker took the thread while this one was exploring the
    /// active thread. It is not explored here, and it is not explored as far
    /// as the active thread is concerned either: the other worker treats the
    /// active thread as explored, and only one of the two can rely on the
    /// other. It becomes `Visited` when the active thread is done.
    Taken,
}

/// What the workers of a search have in common: the work that is waiting
/// for a worker, and which alternatives of the branches that several of them
/// have a copy of are taken.
///
/// Each worker explores a part of the tree of executions, depth first, with
/// a path of its own. When a worker has nothing left, another one hands it
/// an alternative it has not got to yet, along with a copy of the path that
/// leads there. The branches on that path then exist twice, and both
/// workers may find out later, on their own, that one of them needs another
/// thread to be tried: whoever asks here first gets to do it.
///
/// A thread that another worker is exploring from a branch counts as
/// explored there for the purposes of putting threads to sleep. That is
/// sound because claims are made one after the other: of two threads at a
/// branch, only the one claimed later treats the other one as explored.
pub(crate) struct Shared {
    pool: Mutex<Pool>,

    /// Signalled when work arrives, or when there will be none.
    wake: Condvar,

    /// The number of idle workers that no work is waiting for.
    hungry: AtomicUsize,

    /// Set to make every worker stop as soon as it can.
    stop: AtomicBool,

    /// The alternatives (as a bit set) that have been taken, by branch.
    claims: Mutex<HashMap<u64, ThreadSet>>,
}

struct Pool {
    /// Paths that end in an alternative nobody is exploring yet.
    tasks: VecDeque<Path>,

    /// Workers that are waiting for work.
    idle: usize,

    workers: usize,

    /// Set when every worker has run out of work.
    finished: bool,
}

impl Shared {
    pub(crate) fn new(workers: usize) -> Shared {
        Shared {
            pool: Mutex::new(Pool {
                tasks: VecDeque::new(),
                idle: 0,
                workers,
                finished: false,
            }),
            wake: Condvar::new(),
            hungry: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            claims: Mutex::new(HashMap::new()),
        }
    }

    fn pool(&self) -> MutexGuard<'_, Pool> {
        self.pool.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update_hungry(&self, pool: &Pool) {
        self.hungry
            .store(pool.idle.saturating_sub(pool.tasks.len()), Relaxed);
    }

    /// See `Node::claim`.
    fn claim(&self, node: u64, alt: u8) -> Result<ThreadSet, ThreadSet> {
        let mut claims = self.claims.lock().unwrap_or_else(|e| e.into_inner());
        let taken = claims.entry(node).or_insert(0);
        let before = *taken;

        if before & (1 << alt) != 0 {
            return Err(before);
        }

        *taken |= 1 << alt;

        Ok(before)
    }

    /// Records alternatives of a branch as taken.
    fn register(&self, node: u64, alts: ThreadSet) {
        let mut claims = self.claims.lock().unwrap_or_else(|e| e.into_inner());

        *claims.entry(node).or_insert(0) |= alts;
    }

    fn wants_work(&self) -> bool {
        self.hungry.load(Relaxed) > 0 && !self.stopped()
    }

    fn push(&self, path: Path) {
        let mut pool = self.pool();

        pool.tasks.push_back(path);
        self.update_hungry(&pool);
        self.wake.notify_one();
    }

    /// Waits for work. Returns `None` when there is none left for anybody,
    /// or the search has been stopped.
    pub(crate) fn next_task(&self) -> Option<Path> {
        let mut pool = self.pool();

        loop {
            if self.stopped() || pool.finished {
                return None;
            }

            if let Some(path) = pool.tasks.pop_front() {
                self.update_hungry(&pool);
                return Some(path);
            }

            pool.idle += 1;

            if pool.idle == pool.workers {
                // Nobody is left to hand out work.
                pool.finished = true;
                self.wake.notify_all();
                return None;
            }

            self.update_hungry(&pool);

            pool = self.wake.wait(pool).unwrap_or_else(|e| e.into_inner());
            pool.idle -= 1;
        }
    }

    /// Makes every worker stop as soon as it can.
    pub(crate) fn stop(&self) {
        let _pool = self.pool();

        self.stop.store(true, Relaxed);
        self.wake.notify_all();
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Relaxed)
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("Shared").finish_non_exhaustive()
    }
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
            exhaustive: false,
            wakeup_trees: false,
            guide: Wut::default(),
            shared: None,
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
            node: Node::new(),
            values: [0; MAX_ATOMIC_HISTORY],
            pos,
            len: 0,
            guide: self.guide.clone(),
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
                node: Node::new(),
                spur,
                guide: self.guide.clone(),
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
        self.sleep_sets && !self.exhaustive && self.input.is_none() && !self.skipping
    }

    /// Returns `true` if one of `threads` has been explored from the
    /// scheduling decision at `point`, is being explored, or is waiting to
    /// be.
    pub(super) fn is_scheduled(&self, point: usize, threads: ThreadSet) -> bool {
        if point >= self.branches.len() {
            return false;
        }

        let schedule = match object::Ref::from_usize(point, self.branches.id())
            .downcast::<Schedule>(&self.branches)
        {
            Some(schedule) => schedule.get(&self.branches),
            None => return false,
        };

        schedule.exploring
            && schedule.threads.iter().enumerate().any(|(i, th)| {
                threads & (1 << i) != 0
                    && matches!(
                        th,
                        Thread::Active | Thread::Pending | Thread::Visited | Thread::Taken
                    )
            })
    }

    /// Returns `true` if the search is limited to a number of preemptions.
    pub(super) fn is_bounded(&self) -> bool {
        self.preemption_bound.is_some()
    }

    pub(crate) fn set_sleep_sets(&mut self, sleep_sets: bool) {
        self.sleep_sets = sleep_sets;
    }

    pub(crate) fn set_exhaustive(&mut self, exhaustive: bool) {
        self.exhaustive = exhaustive;
    }

    pub(crate) fn set_wakeup_trees(&mut self, wakeup_trees: bool) {
        self.wakeup_trees = wakeup_trees;
    }

    /// Returns `true` if the search uses wakeup trees. They take the place
    /// of adding single threads to scheduling decisions, and build on
    /// threads being put to sleep. With a preemption bound the search is
    /// something else, and with several workers a tree would have to be
    /// shared between the one that explores it and the ones that add to
    /// it: both keep to the older way.
    /// Returns `true` unless exploration has been turned off (see
    /// `loom::stop_exploring`).
    pub(super) fn is_exploring(&self) -> bool {
        self.exploring
    }

    pub(super) fn optimal(&self) -> bool {
        self.wakeup_trees
            && self.sleep_sets
            && !self.exhaustive
            && self.preemption_bound.is_none()
            && self.input.is_none()
            && self.shared.is_none()
    }

    /// Records that `thread`, run from the schedule branch at `pos`, could
    /// carry on after its step.
    pub(super) fn note_carried_on(&mut self, pos: usize, thread: usize) {
        if let Some(schedule) =
            object::Ref::from_usize(pos, self.branches.id()).downcast::<Schedule>(&self.branches)
        {
            schedule.get_mut(&mut self.branches).carried_on |= 1 << thread;
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
    pub(super) fn explored_at(&self, pos: usize, next: usize) -> ThreadSet {
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

                // Other workers may have explored some of what the
                // thread does from here. What they found is not known to
                // this one, so assume the worst.
                let carries_on = schedule.node.shared || schedule.carried_on & (1 << i) != 0;

                preempts(i) as u8 + carries_on as u8 <= preempts(next) as u8
            })
            .fold(0, |visited, (i, _)| visited | 1 << i)
    }

    pub(super) fn branch_thread(
        &mut self,
        execution_id: execution::Id,
        sleeping: ThreadSet,
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
                node: Node::new(),
                preemptions: 0,
                initial_active: None,
                threads: [Thread::Disabled; MAX_THREADS],
                prev,
                sleeping,
                wut: Wut::default(),
                carried_on: 0,
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

            if self.exhaustive && schedule.exploring {
                // Try every thread, unless that would be a preemption and
                // there are none left.
                let out_of_preemptions =
                    self.preemption_bound == Some(preemptions) && initial_active.is_some();

                if !out_of_preemptions {
                    for thread in schedule.threads.iter_mut() {
                        thread.explore();
                    }
                }
            }

            if self.input.is_some() && self.exploring {
                self.select_thread_from_input(schedule_ref);
            }

            self.follow_guide(schedule_ref, sleeping);
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
    /// `point`, or the closest one before it.
    pub(super) fn backtrack(&mut self, mut point: usize, thread_id: thread::Id) {
        if self.input.is_some() {
            return;
        }
        let schedule = loop {
            if let Some(schedule_ref) = object::Ref::from_usize(point, self.branches.id())
                .downcast::<Schedule>(&self.branches)
            {
                let schedule = schedule_ref.get_mut(&mut self.branches);

                if schedule.exploring {
                    schedule.backtrack(thread_id, self.preemption_bound);
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
                        curr.get_mut(&mut self.branches)
                            .backtrack(thread_id, self.preemption_bound);
                        return;
                    }

                    curr = prev;
                } else {
                    if curr.get(&mut self.branches).exploring {
                        // This is the very first schedule
                        curr.get_mut(&mut self.branches)
                            .backtrack(thread_id, self.preemption_bound);
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

        // The next execution goes its own way, unless the branch it starts
        // from says otherwise.
        self.guide = Wut::default();

        // Set the final branch to try the next option. If all options have been
        // traversed, pop the final branch and try again w/ the one under it.
        //
        // This is depth-first tree traversal.
        //
        for index in (0..self.branches.len()).rev() {
            // Remove all objects that were created **after** this branch
            self.branches
                .truncate(object::Ref::from_usize(index, self.branches.id()));

            let found = match self.branches.entry_mut(index) {
                Entry::Schedule(schedule) => {
                    if !schedule.exploring {
                        continue;
                    }

                    // Transition the active thread to visited, and with it
                    // the ones that other workers took in the meantime.
                    for (i, thread) in schedule.threads.iter_mut().enumerate() {
                        if thread.is_active() || *thread == Thread::Taken {
                            *thread = Thread::Visited;
                            schedule.wut.remove(i);
                        }
                    }

                    // Find a pending thread and transition it to active. One
                    // that another worker got to first counts as visited:
                    // that worker explores it.
                    loop {
                        // The sequences of the wakeup tree go first, in the
                        // order in which they were added.
                        let in_tree = schedule
                            .wut
                            .children
                            .iter()
                            .map(|(thread, _)| *thread as usize)
                            .find(|&thread| schedule.threads[thread].is_pending());

                        let pending = match in_tree
                            .or_else(|| schedule.threads.iter().position(|th| th.is_pending()))
                        {
                            Some(pending) => pending,
                            None => break false,
                        };

                        // Whatever other workers took here, they took it
                        // before the thread that is about to be: it is
                        // explored as far as this one is concerned, whether
                        // or not this worker knew that it was wanted.
                        let (claimed, taken) =
                            match schedule.node.claim(&self.shared, pending as u8) {
                                Ok(taken) => (true, taken),
                                Err(taken) => (false, taken),
                            };

                        for (i, thread) in schedule.threads.iter_mut().enumerate() {
                            if taken & (1 << i) != 0 {
                                *thread = Thread::Visited;
                            }
                        }

                        if claimed {
                            schedule.threads[pending] = Thread::Active;
                            self.guide = schedule.wut.subtree(pending);
                            break true;
                        }
                    }
                }
                Entry::Load(load) => {
                    if !load.exploring {
                        continue;
                    }

                    loop {
                        load.pos += 1;

                        if load.pos >= load.len {
                            break false;
                        }

                        if load.node.claim(&self.shared, load.pos).is_ok() {
                            self.guide = load.guide.clone();
                            break true;
                        }
                    }
                }
                Entry::Spurious(spurious) => {
                    if !spurious.exploring || spurious.spur {
                        continue;
                    }

                    spurious.spur = true;
                    self.guide = spurious.guide.clone();
                    spurious.node.claim(&self.shared, 1).is_ok()
                }
            };

            if found {
                self.offer_work();
                return true;
            }
        }

        false
    }

    /// Makes the scheduling decision that has just been created go the way
    /// the wakeup tree it was created under says, if there is one.
    fn follow_guide(&mut self, schedule_ref: object::Ref<Schedule>, sleeping: ThreadSet) {
        if self.guide.is_empty() {
            return;
        }

        let guide = std::mem::take(&mut self.guide);

        if !self.optimal() {
            return;
        }

        let schedule = schedule_ref.get_mut(&mut self.branches);

        if !schedule.exploring {
            return;
        }

        let mut guide = guide;

        let waits = guide.children.iter().any(|(thread, _)| {
            !matches!(
                schedule.threads[*thread as usize],
                Thread::Active | Thread::Skip
            )
        });

        if waits {
            // The execution is not where a sequence was meant for: its
            // thread waits for something. Whatever the sequence was after
            // has to be found some other way, so every thread gets its turn
            // from here.
            for (i, thread) in schedule.threads.iter_mut().enumerate() {
                if sleeping & (1 << i) == 0 {
                    thread.explore();
                }
            }

            return;
        }

        // Where a sequence leads whose thread is asleep has been explored.
        // (That happens when what an operation has to do with another one
        // depends on what came before, and came out differently this time.)
        guide
            .children
            .retain(|(thread, _)| sleeping & (1 << *thread) == 0);

        if guide.is_empty() {
            // There is nothing to explore down this way.
            schedule.exploring = false;
            self.skip_branch();

            return;
        }

        for thread in schedule.threads.iter_mut() {
            if thread.is_active() {
                *thread = Thread::Skip;
            }
        }

        // Every sequence of the tree starts from here, the first one first.
        for (thread, _) in &guide.children {
            schedule.threads[*thread as usize] = Thread::Pending;
        }

        let first = guide.children[0].0 as usize;

        schedule.threads[first] = Thread::Active;
        self.guide = guide.subtree(first);
        schedule.wut = guide;
    }

    /// Adds the sequence `steps` to the wakeup tree of the scheduling
    /// decision at `point`, unless the executions that go on from there with
    /// those steps are taken care of: by a thread that has been explored
    /// from there, or by a sequence that is in the tree.
    ///
    /// `before(a, b)` says whether the step identified by `a` has to come
    /// before the one identified by `b`, for two steps of the sequence.
    ///
    /// Returns `false` if there is no such decision to add anything to.
    pub(super) fn insert_wakeup(
        &mut self,
        point: usize,
        steps: &[Wakeup],
        before: impl Fn(usize, usize) -> bool,
    ) -> bool {
        if point >= self.branches.len() || steps.is_empty() {
            return false;
        }

        let schedule = match object::Ref::from_usize(point, self.branches.id())
            .downcast::<Schedule>(&self.branches)
        {
            Some(schedule) => schedule.get_mut(&mut self.branches),
            None => return false,
        };

        if !schedule.exploring {
            return false;
        }

        // The positions of the steps that can be the first of the sequence:
        // each thread's first step, if no step of another thread has to
        // come before it.
        let initials = |steps: &[Wakeup]| {
            let mut seen: ThreadSet = 0;
            let mut initials = Vec::new();

            for (i, step) in steps.iter().enumerate() {
                if seen & (1 << step.thread) != 0 {
                    continue;
                }

                seen |= 1 << step.thread;

                let leads = steps[..i].iter().all(|earlier| {
                    earlier.thread == step.thread || !before(earlier.step, step.step)
                });

                if leads {
                    initials.push(i);
                }
            }

            initials
        };

        let asleep = schedule
            .threads
            .iter()
            .enumerate()
            .filter(|(_, th)| **th == Thread::Visited)
            .fold(schedule.sleeping, |asleep, (i, _)| asleep | 1 << i);

        let mut steps = steps.to_vec();

        // Running a thread that is asleep here first leads to nothing new,
        // and the sequence can be made to start with it.
        if initials(&steps)
            .iter()
            .any(|&i| asleep & (1 << steps[i].thread) != 0)
        {
            return true;
        }

        let Schedule { threads, wut, .. } = schedule;

        let mut tree = wut;
        let mut root = true;

        loop {
            let first = initials(&steps);

            // A sequence of the tree that this one can be made to start
            // with: the oldest one.
            let child = tree.children.iter().position(|(thread, _)| {
                !(root && threads[*thread as usize].is_active())
                    && first.iter().any(|&i| steps[i].thread == *thread as usize)
            });

            let child = match child {
                Some(child) => child,
                None => break,
            };

            let thread = tree.children[child].0 as usize;
            let position = first
                .iter()
                .copied()
                .find(|&i| steps[i].thread == thread)
                .unwrap();

            steps.remove(position);

            if steps.is_empty() || tree.children[child].1.is_empty() {
                // The tree has the sequence, or one that ends earlier and
                // leaves the search free to go on like this one.
                return true;
            }

            tree = &mut tree.children[child].1;
            root = false;
        }

        if root {
            let first = steps[0].thread;

            match threads[first] {
                Thread::Skip | Thread::Pending => threads[first] = Thread::Pending,
                Thread::Active | Thread::Visited | Thread::Taken => return true,
                Thread::Disabled | Thread::Yield => {
                    // The thread cannot run here. As when a single thread
                    // is asked for that cannot: every thread gets its turn.
                    for (i, thread) in threads.iter_mut().enumerate() {
                        if asleep & (1 << i) == 0 {
                            thread.explore();
                        }
                    }

                    return true;
                }
            }
        }

        tree.children.extend(Wut::sequence(&steps).children);

        true
    }

    /// Takes part in a search that several workers run together.
    pub(crate) fn set_shared(&mut self, shared: Arc<Shared>) {
        self.shared = Some(shared);
    }

    /// Hands alternatives that this path has yet to explore to workers that
    /// have run out of work, as long as there are both.
    fn offer_work(&mut self) {
        let shared = match &self.shared {
            Some(shared) => shared.clone(),
            None => return,
        };

        let mut from = 0;

        while shared.wants_work() {
            match self.split(&shared, &mut from) {
                Some(path) => shared.push(path),
                None => break,
            }
        }
    }

    /// Splits off the first alternative that is left to explore at or after
    /// the branch at `from`: the closer to the start, the more work there
    /// is likely to be behind it. Returns the path that leads to it.
    fn split(&mut self, shared: &Arc<Shared>, from: &mut usize) -> Option<Path> {
        for index in *from..self.branches.len() {
            loop {
                let (node, alt) = match self.branches.entry(index) {
                    Entry::Schedule(schedule) if schedule.exploring => {
                        match schedule.threads.iter().position(|th| th.is_pending()) {
                            Some(pending) => (schedule.node, pending as u8),
                            None => break,
                        }
                    }
                    Entry::Load(load) if load.exploring && load.pos + 1 < load.len => {
                        (load.node, load.pos + 1)
                    }
                    Entry::Spurious(spurious) if spurious.exploring && !spurious.spur => {
                        (spurious.node, 1)
                    }
                    _ => break,
                };

                // The other worker gets a copy of everything up to here.
                // From now on these branches are worked on by both.
                for shared_index in 0..=index {
                    match self.branches.entry_mut(shared_index) {
                        Entry::Schedule(schedule) => {
                            if !schedule.node.shared {
                                // Let the others know what has been
                                // explored here, or is being.
                                let explored = schedule
                                    .threads
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, th)| th.is_active() || **th == Thread::Visited)
                                    .fold(0, |explored, (i, _)| explored | 1 << i);

                                shared.register(schedule.node.id, explored);
                            }

                            schedule.node.shared = true;
                        }
                        Entry::Load(load) => load.node.shared = true,
                        Entry::Spurious(spurious) => spurious.node.shared = true,
                    }
                }

                let taken = match shared.claim(node.id, alt) {
                    Ok(taken) => taken,
                    Err(_) => {
                        // Somebody else has it.
                        match self.branches.entry_mut(index) {
                            Entry::Schedule(schedule) => {
                                schedule.threads[alt as usize] = Thread::Taken;
                                continue;
                            }
                            _ => break,
                        }
                    }
                };

                let mut branches = self.branches.clone_prefix(index + 1);

                match branches.entry_mut(index) {
                    Entry::Schedule(schedule) => {
                        // What this worker is exploring from here is as good
                        // as explored for the other one, and so is what
                        // was taken before.
                        for (i, thread) in schedule.threads.iter_mut().enumerate() {
                            if thread.is_active() || taken & (1 << i) != 0 {
                                *thread = Thread::Visited;
                            }
                        }

                        schedule.threads[alt as usize] = Thread::Active;
                    }
                    Entry::Load(load) => load.pos = alt,
                    Entry::Spurious(spurious) => spurious.spur = true,
                }

                if let Entry::Schedule(schedule) = self.branches.entry_mut(index) {
                    schedule.threads[alt as usize] = Thread::Taken;
                }

                *from = index;

                return Some(Path {
                    input: None,
                    preemption_bound: self.preemption_bound,
                    pos: 0,
                    branches,
                    exploring: self.exploring_on_start,
                    skipping: false,
                    exploring_on_start: self.exploring_on_start,
                    sleep_sets: self.sleep_sets,
                    exhaustive: self.exhaustive,
                    wakeup_trees: self.wakeup_trees,
                    guide: Wut::default(),
                    shared: Some(shared.clone()),
                });
            }
        }

        None
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

    fn backtrack(&mut self, thread_id: thread::Id, preemption_bound: Option<u8>) {
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
        //
        // That holds even if the step that is wanted earlier is the thread's
        // next one, which is what being asleep is about. The executions in
        // which that step comes first have been explored, but the search
        // does not get from them to everything that follows from the two
        // operations being the other way around here: it relies on going
        // through the reordering at this point for that.
        let asleep = self.sleeping & (1 << thread_id) != 0;

        // A thread that has yielded and is waiting for the others to take a
        // step cannot run here any more than one that is blocked.
        let can_run =
            self.threads[thread_id].is_enabled() && self.threads[thread_id] != Thread::Yield;

        if can_run && !asleep {
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
