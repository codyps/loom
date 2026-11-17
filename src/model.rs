//! Model concurrent programs.

use crate::rt::speculation::{self, LoadBuffering, Pass};
use crate::rt::{self, Execution, Scheduler};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, subscriber};
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_MAX_THREADS: usize = crate::MAX_THREADS;
const DEFAULT_MAX_BRANCHES: usize = 1_000;

/// Configure a model
#[derive(Debug)]
#[non_exhaustive] // Support adding more fields in the future
pub struct Builder {
    /// Max number of threads to check as part of the execution, including
    /// the main thread.
    ///
    /// Defaults to [`MAX_THREADS`](crate::MAX_THREADS), which is also the
    /// most it can be. A lower value makes a model that spawns more threads
    /// than intended fail instead of taking forever.
    pub max_threads: usize,

    /// The number of stores to an atomic that loom keeps, and that a load
    /// can still return.
    ///
    /// A load may return any store that the loading thread does not know to
    /// be outdated, which can be one made long ago. Loom only keeps so many
    /// stores of each atomic. When another one is made, the store that is
    /// earliest in the order of the atomic's stores is given up: it is not
    /// returned anymore, and the executions in which it would be are not
    /// explored. If a load can return no other store, the model fails with
    /// a message that says so. Raising the number explores those
    /// executions, and makes every operation on an atomic that has been
    /// stored to that often take longer.
    ///
    /// Keeping very few stores can also make loom report an outcome that no
    /// execution has: a store that is given up while what it is ordered with
    /// is still undecided takes part of that order with it. On the litmus
    /// corpus this happens with 2, for 3 of its 23,715 programs, and not with
    /// 1, nor with 3 or more.
    ///
    /// Defaults to the `LOOM_ATOMIC_HISTORY` environment variable, or 7.
    /// At most 16.
    pub atomic_history: usize,

    /// Maximum number of thread switches per permutation.
    ///
    /// Defaults to `LOOM_MAX_BRANCHES` environment variable.
    pub max_branches: usize,

    /// Maximum number of permutations to explore.
    ///
    /// Defaults to `LOOM_MAX_PERMUTATIONS` environment variable.
    pub max_permutations: Option<usize>,

    /// Maximum amount of time to spend on checking
    ///
    /// Defaults to `LOOM_MAX_DURATION` environment variable.
    pub max_duration: Option<Duration>,

    /// Default stack size for the model's threads.
    ///
    /// This applies to the main thread and any spawned threads.
    /// [`thread::Builder::stack_size`](crate::thread::Builder::stack_size)
    /// overrides it for an individual thread.
    ///
    /// The value is handed to the underlying coroutine library as is, which
    /// measures it in machine words (`usize`), not bytes: on a 64-bit target
    /// `Some(0x2000)` requests a 64 KiB stack.
    ///
    /// This should be kept low but may need to be increased if encountering a
    /// stack overflow error:
    ///
    /// > coroutine in thread '[...]' has overflowed its stack
    ///
    /// Defaults to the `LOOM_STACK_SIZE` environment variable if set. If
    /// `None`, the coroutine library's default is used
    /// (`generator::DEFAULT_STACK_SIZE`).
    pub stack_size: Option<usize>,

    /// Maximum number of thread preemptions to explore
    ///
    /// Defaults to `LOOM_MAX_PREEMPTIONS` environment variable.
    pub preemption_bound: Option<usize>,

    /// When doing an exhaustive check, uses the file to store and load the
    /// check progress
    ///
    /// A checkpoint records which model it belongs to (identified by the
    /// type of the closure passed to [`check`](Builder::check)), and a
    /// checkpoint left behind by a different model is ignored rather than
    /// replayed against a model it does not fit.
    ///
    /// If the path is an existing directory, or ends with a path separator,
    /// each model gets its own file in that directory. This is what to use
    /// when several tests run with checkpointing enabled, since with a single
    /// file each test overwrites the checkpoint of the previous one.
    ///
    /// Defaults to `LOOM_CHECKPOINT_FILE` environment variable.
    pub checkpoint_file: Option<PathBuf>,

    /// How often to write the checkpoint file
    ///
    /// Defaults to `LOOM_CHECKPOINT_INTERVAL` environment variable.
    pub checkpoint_interval: usize,

    /// When `true` loom won't start state exploration until `explore_state` is
    /// called.
    pub expect_explicit_explore: bool,

    /// When `true`, locations are captured on each loom operation.
    ///
    /// Note that is is **very** expensive. It is recommended to first isolate a
    /// failing iteration using `LOOM_CHECKPOINT_FILE`, then enable location
    /// tracking.
    ///
    /// Defaults to `LOOM_LOCATION` environment variable.
    pub location: bool,

    /// Log execution output to stdout.
    ///
    /// Defaults to existence of `LOOM_LOG` environment variable.
    pub log: bool,

    /// Skip executions that can only repeat ones already explored.
    ///
    /// Once every execution in which some thread runs next has been explored
    /// from a point, loom remembers, further down every other path from that
    /// point, that running this thread leads to nothing new, until something
    /// happens that the thread's next operation depends on. (These are the
    /// "sleep sets" of partial order reduction.) This does not change what
    /// loom finds, only how many executions it takes, often by orders of
    /// magnitude.
    ///
    /// With a preemption bound it does less: a bounded search does not
    /// explore everything below a point, so a thread is only left out where
    /// the executions that were explored had at least as many preemptions to
    /// spare as the ones being skipped.
    ///
    /// Defaults to `true`. Setting the `LOOM_SLEEP_SETS` environment variable
    /// to `0` turns it off.
    pub sleep_sets: bool,

    /// Explore each way of ordering the operations of a model that have to
    /// do with each other exactly once, and nothing else.
    ///
    /// Without this, loom tries a thread from an earlier point when two
    /// operations could have been the other way around, and sees where that
    /// leads. Often it leads, many steps later, to an execution that has
    /// been explored. With it, loom keeps track of the steps that get the
    /// two operations the other way around, and follows them (the "wakeup
    /// trees" of optimal dynamic partial order reduction).
    ///
    /// This builds on [`sleep_sets`](Builder::sleep_sets), and is not used
    /// with a preemption bound or with more than one
    /// [worker](Builder::workers).
    ///
    /// Defaults to `true`. Setting the `LOOM_WAKEUP_TREES` environment
    /// variable to `0` turns it off.
    pub wakeup_trees: bool,

    /// Let a [`Condvar::wait`](crate::sync::Condvar::wait) end without a
    /// notification, which `std` allows: a spurious wakeup. Code that waits
    /// has to check, after the wait, that what it waited for has happened,
    /// in a loop, as [`Condvar::wait_while`](crate::sync::Condvar::wait_while)
    /// does. Each thread wakes up spuriously at most once per condition
    /// variable in an execution, so that such loops end.
    ///
    /// A timed wait can always time out, whatever this says.
    ///
    /// Defaults to `true`. Setting the `LOOM_SPURIOUS_WAKEUPS` environment
    /// variable to `0` turns it off.
    pub spurious_wakeups: bool,

    /// Let a `Relaxed` load return a store that comes after it: "load
    /// buffering", which the C++ standard allows, and hardware such as Arm
    /// does, but loom's model of memory (RC11) does not. In
    ///
    /// ```text
    /// thread 1: let a = x.load(Relaxed); y.store(1, Relaxed);
    /// thread 2: let b = y.load(Relaxed); x.store(b, Relaxed);
    /// ```
    ///
    /// `a` can be 1: thread 1's load reads thread 2's store, which copies
    /// the value thread 1 stores after its load.
    ///
    /// Loom runs an execution one step after the other, so it guesses such
    /// a value and checks the guess when the store is made. The model is
    /// explored twice: the usual way, noting the values each atomic is
    /// given, and then with `Relaxed` loads that may also return one of
    /// those values, from a store to come. Executions in which the guess
    /// does not come true are dropped; a panic while a guess is still open
    /// is only reported once it has. This takes longer than twice as long,
    /// and finds some of what load buffering allows, not all of it:
    ///
    /// - Only plain `Relaxed` loads guess, and only values that an atomic
    ///   is given in an execution without guesses.
    /// - Until its guess comes true, the loading thread may not access that
    ///   atomic again, nor execute an acquire or `SeqCst` fence; an
    ///   execution in which it does is dropped.
    ///
    /// What the model does outside of loom is not undone for an execution
    /// that is dropped. A model that records values in a `std` collection
    /// has to check [`is_settled`] first.
    ///
    /// Not used with several [`workers`](Builder::workers), a
    /// [checkpoint file](Builder::checkpoint_file), or
    /// [`check_with_input`](Builder::check_with_input).
    ///
    /// Defaults to `false`. Setting the `LOOM_LOAD_BUFFERING` environment
    /// variable to `1` turns it on.
    pub load_buffering: bool,

    /// Try every thread at every point where the preemption bound allows
    /// it, instead of only the ones that loom can tell make a difference.
    ///
    /// This is for testing loom: the outcomes of a search like that are
    /// what the usual one has to find as well. It is far too slow for
    /// anything but the smallest models.
    #[doc(hidden)]
    pub exhaustive: bool,

    /// The number of OS threads to explore executions with.
    ///
    /// With more than one, the executions of a model are divided among that
    /// many workers, and the closure passed to [`check`](Builder::check) is
    /// called from all of them at the same time. Each call is still an
    /// execution of its own, with its own loom objects, but whatever the
    /// closure shares between executions (a counter of executions, a set of
    /// observed results) is now shared between threads, and the order in
    /// which executions are run is no longer the same from run to run.
    ///
    /// If the model fails, the other workers stop after the execution they
    /// are in, and the failure is passed on to the caller of `check`. Which
    /// failing execution that is may differ between runs.
    ///
    /// A single worker is used regardless of this setting when a checkpoint
    /// file is in use. Log output (`LOOM_LOG`) only covers the worker that
    /// runs on the calling thread.
    ///
    /// Defaults to the `LOOM_WORKERS` environment variable, or 1.
    pub workers: usize,
}

impl Builder {
    /// Create a new `Builder` instance with default values.
    pub fn new() -> Builder {
        use std::env;

        let checkpoint_interval = env::var("LOOM_CHECKPOINT_INTERVAL")
            .map(|v| {
                v.parse()
                    .expect("invalid value for `LOOM_CHECKPOINT_INTERVAL`")
            })
            .unwrap_or(20_000);

        let max_branches = env::var("LOOM_MAX_BRANCHES")
            .map(|v| v.parse().expect("invalid value for `LOOM_MAX_BRANCHES`"))
            .unwrap_or(DEFAULT_MAX_BRANCHES);

        let location = env::var("LOOM_LOCATION").is_ok();

        let log = env::var("LOOM_LOG").is_ok();

        let sleep_sets = env::var("LOOM_SLEEP_SETS").map_or(true, |v| v != "0");

        let atomic_history = env::var("LOOM_ATOMIC_HISTORY")
            .map(|v| v.parse().expect("invalid value for `LOOM_ATOMIC_HISTORY`"))
            .unwrap_or(7);

        let wakeup_trees = env::var("LOOM_WAKEUP_TREES").map_or(true, |v| v != "0");

        let spurious_wakeups = env::var("LOOM_SPURIOUS_WAKEUPS").map_or(true, |v| v != "0");

        let load_buffering = env::var("LOOM_LOAD_BUFFERING").map_or(false, |v| v == "1");

        let workers = env::var("LOOM_WORKERS")
            .map(|v| v.parse().expect("invalid value for `LOOM_WORKERS`"))
            .unwrap_or(1);

        let max_duration = env::var("LOOM_MAX_DURATION")
            .map(|v| {
                let secs = v.parse().expect("invalid value for `LOOM_MAX_DURATION`");
                Duration::from_secs(secs)
            })
            .ok();

        let max_permutations = env::var("LOOM_MAX_PERMUTATIONS")
            .map(|v| {
                v.parse()
                    .expect("invalid value for `LOOM_MAX_PERMUTATIONS`")
            })
            .ok();

        let stack_size = env::var("LOOM_STACK_SIZE")
            .map(|v| v.parse().expect("invalid value for `LOOM_STACK_SIZE`"))
            .ok();

        let preemption_bound = env::var("LOOM_MAX_PREEMPTIONS")
            .map(|v| v.parse().expect("invalid value for `LOOM_MAX_PREEMPTIONS`"))
            .ok();

        let checkpoint_file = env::var("LOOM_CHECKPOINT_FILE")
            .map(|v| v.parse().expect("invalid value for `LOOM_CHECKPOINT_FILE`"))
            .ok();

        Builder {
            max_threads: DEFAULT_MAX_THREADS,
            atomic_history,
            max_branches,
            max_duration,
            max_permutations,
            stack_size,
            preemption_bound,
            checkpoint_file,
            checkpoint_interval,
            expect_explicit_explore: false,
            location,
            log,
            sleep_sets,
            wakeup_trees,
            spurious_wakeups,
            load_buffering,
            exhaustive: false,
            workers,
        }
    }

    /// Set the checkpoint file.
    pub fn checkpoint_file(&mut self, file: &str) -> &mut Self {
        self.checkpoint_file = Some(file.into());
        self
    }

    /// Run one execution of the model, using bytes to choose its execution path.
    ///
    /// At each choice with multiple eligible outcomes, consumes one byte and
    /// selects `byte % number_of_outcomes`. Thread choices are ordered by Loom
    /// thread ID. Atomic read outcomes (including read-modify-write operations)
    /// and spurious wakeups use the same input stream.
    /// Choices with a single outcome consume no input. Exhausted input selects
    /// the first outcome, and unused bytes are ignored.
    ///
    /// The model must be deterministic apart from Loom's modeled operations for
    /// the same input to reproduce an execution. Replay across Loom versions is
    /// not guaranteed. Input is copied into the execution before the model runs.
    ///
    /// Respects `max_threads`, `max_branches`, `preemption_bound`, `stack_size`,
    /// `location`, `log`, and explicit exploration boundaries. While exploration is disabled,
    /// Loom uses its usual initial choices without consuming input.
    /// Checkpoint and permutation settings, including `max_duration`, are unused
    /// because this method runs only one execution.
    ///
    /// # Panics
    ///
    /// Model failures and exceeded execution limits panic as in [`Builder::check`].
    ///
    /// No input can starve a thread that waits by yielding (`thread::yield_now`
    /// or `hint::spin_loop`): a thread that yields is not chosen again before
    /// every other thread that can run has taken a step, whatever the input
    /// says, and the same holds for the first outcomes chosen once the input
    /// runs out. A waiting loop that does not yield can run forever and exceed
    /// `max_branches` here, as it can with [`Builder::check`].
    ///
    /// ```
    /// let input = [5, 2];
    /// loom::model::Builder::new().check_with_input(&input, || {
    ///     let child = loom::thread::spawn(|| {});
    ///     child.join().unwrap();
    /// });
    /// ```
    pub fn check_with_input<F>(&self, input: &[u8], f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.with_logging(|| self.check_with_input_inner(input, f))
    }

    fn check_with_input_inner<F>(&self, input: &[u8], f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let _span = tracing::info_span!("iter", message = 1).entered();
        let mut execution = self.new_execution();
        execution.path.set_input(input);

        let mut scheduler = Scheduler::new(self.max_threads);
        run_execution(&mut execution, &mut scheduler, self.stack_size, f);
    }

    /// Check the provided model.
    ///
    /// Unless a [`tracing`] subscriber is already active for the calling
    /// thread, loom installs its own for the duration of the check, filtered
    /// by the `LOOM_LOG` environment variable. A subscriber set up by the
    /// caller is left in place and receives loom's events instead.
    pub fn check<F>(&self, f: F)
    where
        F: Fn() + Sync + Send + 'static,
    {
        self.with_logging(|| self.check_inner(f))
    }

    /// Runs `f` with loom's `LOOM_LOG` subscriber installed, unless the
    /// caller already has a subscriber of their own in place.
    fn with_logging<R>(&self, f: impl FnOnce() -> R) -> R {
        let has_subscriber =
            tracing::dispatcher::get_default(|current| !current.is::<subscriber::NoSubscriber>());

        if has_subscriber {
            return f();
        }

        let subscriber = fmt::Subscriber::builder()
            .with_env_filter(EnvFilter::from_env("LOOM_LOG"))
            .with_test_writer()
            .without_time()
            .finish();

        subscriber::with_default(subscriber, f)
    }

    fn check_inner<F>(&self, f: F)
    where
        F: Fn() + Sync + Send + 'static,
    {
        if self.workers > 1 && self.checkpoint_file.is_none() && !self.load_buffering {
            return self.check_parallel(f);
        }

        let mut i = 1;
        let mut _span = tracing::info_span!("iter", message = i).entered();

        let mut execution = self.new_execution();
        let mut scheduler = Scheduler::new(self.max_threads);

        if self.load_buffering {
            execution.load_buffering = Some(Box::new(LoadBuffering::new()));
        }

        // Identifies the model a checkpoint belongs to.
        let model = std::any::type_name::<F>();

        // A checkpoint records where the search is, not which of the two
        // passes of load buffering it is in.
        let checkpoint_file = self
            .checkpoint_file
            .as_ref()
            .filter(|_| !self.load_buffering)
            .map(|path| checkpoint::file_for(path, model));

        if let Some(ref path) = checkpoint_file {
            if path.exists() {
                if let Some(checkpoint) = checkpoint::load_execution_path(path, model) {
                    execution.path = checkpoint;
                    execution.path.set_max_branches(self.max_branches);
                    execution.path.set_sleep_sets(self.sleep_sets);
                    execution.path.set_exhaustive(self.exhaustive);
                    execution.path.set_wakeup_trees(self.wakeup_trees);
                }
            }
        }

        let f = Arc::new(f);

        let start = Instant::now();
        loop {
            if i % self.checkpoint_interval == 0 {
                info!(parent: None, "");
                info!(
                    parent: None,
                    " ================== Iteration {} ==================", i
                );
                info!(parent: None, "");

                if let Some(ref path) = checkpoint_file {
                    checkpoint::store_execution_path(&execution.path, path, model);
                }

                if let Some(max_permutations) = self.max_permutations {
                    if i >= max_permutations {
                        return;
                    }
                }

                if let Some(max_duration) = self.max_duration {
                    if start.elapsed() >= max_duration {
                        return;
                    }
                }
            }

            let f = f.clone();

            run_execution(&mut execution, &mut scheduler, self.stack_size, move || f());

            i += 1;

            // Create the next iteration's `tracing` span before trying to step to the next
            // execution, as the `Execution` will capture the current span when
            // it's reset.
            _span = tracing::info_span!(parent: None, "iter", message = i).entered();
            // What is known of load buffering carries over to the next
            // execution, or to the second pass.
            let load_buffering = execution.load_buffering.take();

            if let Some(mut next) = execution.step() {
                next.load_buffering = load_buffering.map(|mut load_buffering| {
                    load_buffering.reset();
                    load_buffering
                });
                execution = next;
            } else if let Some(mut load_buffering) =
                load_buffering.filter(|load_buffering| load_buffering.pass == Pass::Collect)
            {
                // Explore again, guessing values for loads from the ones
                // noted (see `rt::speculation`).
                info!(parent: None, "Exploring load buffering after {} iterations", i - 1);

                load_buffering.pass = Pass::Guess;
                load_buffering.reset();

                execution = self.new_execution();
                execution.load_buffering = Some(load_buffering);
            } else {
                info!(parent: None, "Completed in {} iterations", i - 1);
                return;
            }
        }
    }
}

impl Builder {
    /// Explores the model with several workers, each on a thread of its
    /// own, see `rt::Shared` for how the work is divided.
    fn check_parallel<F>(&self, f: F)
    where
        F: Fn() + Sync + Send + 'static,
    {
        use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        use std::sync::Mutex;

        let f = Arc::new(f);
        let shared = Arc::new(rt::Shared::new(self.workers));
        let iterations = AtomicUsize::new(0);
        let failure = Mutex::new(None);
        let start = Instant::now();

        let worker = |first: bool| {
            let mut scheduler = Scheduler::new(self.max_threads);

            // One worker starts at the root. The others wait for it to
            // find something for them to do.
            let mut task = if first {
                Some(self.new_execution().path)
            } else {
                shared.next_task()
            };

            while let Some(mut path) = task {
                path.set_shared(shared.clone());

                let mut execution = self.new_execution();
                execution.path = path;

                loop {
                    if shared.stopped() {
                        return;
                    }

                    let i = iterations.fetch_add(1, Relaxed) + 1;

                    if i % self.checkpoint_interval == 0 {
                        info!(parent: None, " ================== Iteration {} ==================", i);

                        let out_of_time = self
                            .max_duration
                            .map_or(false, |max| start.elapsed() >= max);
                        let out_of_permutations =
                            self.max_permutations.map_or(false, |max| i >= max);

                        if out_of_time || out_of_permutations {
                            shared.stop();
                            return;
                        }
                    }

                    let _span = tracing::info_span!(parent: None, "iter", message = i).entered();

                    let f = f.clone();

                    run_execution(&mut execution, &mut scheduler, self.stack_size, move || f());

                    match execution.step() {
                        Some(next) => execution = next,
                        None => break,
                    }
                }

                task = shared.next_task();
            }
        };

        let run = |first: bool| {
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| worker(first))) {
                // The first failure is the one that is reported.
                failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(panic);
                shared.stop();
            }
        };

        std::thread::scope(|scope| {
            for worker in 1..self.workers {
                std::thread::Builder::new()
                    .name(format!("loom-worker-{}", worker))
                    .spawn_scoped(scope, || run(false))
                    .expect("failed to spawn a loom worker thread");
            }

            run(true);
        });

        let failure = failure.lock().unwrap_or_else(|e| e.into_inner()).take();

        if let Some(panic) = failure {
            resume_unwind(panic);
        }

        info!(parent: None, "Completed in {} iterations", iterations.load(Relaxed));
    }

    fn new_execution(&self) -> Execution {
        assert!(
            (1..=crate::MAX_THREADS).contains(&self.max_threads),
            "`max_threads` has to be between 1 and {}",
            crate::MAX_THREADS
        );

        // Checked here, outside of the model: in the model, the panic would
        // need room on a thread's small stack to capture a backtrace.
        assert!(
            (1..=crate::rt::MAX_ATOMIC_HISTORY).contains(&self.atomic_history),
            "the number of stores kept per atomic has to be between 1 and {}",
            crate::rt::MAX_ATOMIC_HISTORY
        );

        let mut execution = Execution::new(
            self.max_threads,
            self.max_branches,
            self.preemption_bound,
            !self.expect_explicit_explore,
        );

        execution.log = self.log;
        execution.location = self.location;
        execution.max_history = self.atomic_history;
        execution.spurious_wakeups = self.spurious_wakeups;
        execution.path.set_sleep_sets(self.sleep_sets);
        execution.path.set_exhaustive(self.exhaustive);
        execution.path.set_wakeup_trees(self.wakeup_trees);

        execution
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

fn run_execution<F>(
    execution: &mut Execution,
    scheduler: &mut Scheduler,
    stack_size: Option<usize>,
    f: F,
) where
    F: FnOnce() + Send + 'static,
{
    scheduler.run(execution, stack_size, move || {
        /// the given closure `f` may panic when executed.
        /// when this happens, we still want to ensure
        /// that thread locals are destructed before the
        /// global statics are dropped. therefore, we set
        /// up a guard that is dropped as part of the unwind
        /// logic when `f` panics. this guard in turn ensures,
        /// through the implementation of `rt::thread_done`,
        /// that thread local fields are dropped before the
        /// global statics are dropped. without this guard,
        /// a `Drop` implementation on a type that is stored
        /// in a thread local field could access the lazy static,
        /// and this then would in turn panic from the method
        /// [`Lazy::get`](crate::lazy_static::Lazy), which
        /// causes a panic inside a panic, which in turn causes
        /// Rust to abort, triggering a segfault in `loom`.
        struct PanicGuard;
        impl Drop for PanicGuard {
            fn drop(&mut self) {
                rt::thread_done(true);
            }
        }

        // set up the panic guard
        let panic_guard = PanicGuard;

        // execute the closure, note that `f()` may panic!
        speculation::run_thread(f);

        // if `f()` didn't panic, then we terminate the
        // main thread by dropping the guard ourselves.
        drop(panic_guard);
    });

    if let Some(load_buffering) = &mut execution.load_buffering {
        match load_buffering.outcome() {
            // The execution does not exist: a guess did not come true.
            None => return,
            Some(Some(failure)) => std::panic::resume_unwind(failure),
            Some(None) => {}
        }
    }

    execution.check_for_leaks();
}

/// Returns `false` while a load of the current execution has returned a
/// value that no store has given it yet, which only happens with
/// [`Builder::load_buffering`]. An execution that ends that way does not
/// exist, and is dropped. A model that records what it sees outside of loom,
/// in a `std` collection, say, has to leave out what it sees then.
///
/// Must be called from within a model.
pub fn is_settled() -> bool {
    rt::execution(|execution| {
        execution
            .load_buffering
            .as_ref()
            .map_or(true, |load_buffering| {
                load_buffering.pending.is_empty() && !load_buffering.discarding
            })
    })
}

/// Run all concurrent permutations of the provided closure.
///
/// Uses a default [`Builder`] which can be affected by environment variables.
pub fn model<F>(f: F)
where
    F: Fn() + Sync + Send + 'static,
{
    Builder::new().check(f)
}

#[cfg(feature = "checkpoint")]
mod checkpoint {
    use serde::{Deserialize, Serialize};
    use std::fs::{self, File};
    use std::io::prelude::*;
    use std::path::{Path, PathBuf, MAIN_SEPARATOR};

    #[derive(Serialize)]
    struct Store<'a> {
        model: &'a str,
        path: &'a crate::rt::Path,
    }

    #[derive(Deserialize)]
    struct Load {
        model: String,
        path: crate::rt::Path,
    }

    /// Returns the file to use for `model`'s checkpoint: `path` itself, or a
    /// file named after the model if `path` is a directory.
    pub(crate) fn file_for(path: &Path, model: &str) -> PathBuf {
        let is_dir = path.is_dir() || path.to_string_lossy().ends_with(['/', MAIN_SEPARATOR]);

        if !is_dir {
            return path.to_path_buf();
        }

        let name: String = model
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();

        path.join(format!("{}.json", name))
    }

    /// Loads the execution path stored at `fs_path`, unless it was stored by
    /// a different model.
    pub(crate) fn load_execution_path(fs_path: &Path, model: &str) -> Option<crate::rt::Path> {
        let mut file = File::open(fs_path).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();

        match serde_json::from_str::<Load>(&contents) {
            Ok(load) if load.model == model => Some(load.path),
            Ok(load) => {
                // Replaying another model's path makes no sense: at best it
                // fails right away, at worst it explores something arbitrary.
                eprintln!(
                    "loom: ignoring checkpoint `{}`: it belongs to `{}`, not to `{}`",
                    fs_path.display(),
                    load.model,
                    model
                );
                None
            }
            // A checkpoint from before the model was recorded.
            Err(_) => Some(serde_json::from_str(&contents).unwrap_or_else(|err| {
                panic!(
                    "loom: cannot read checkpoint `{}`: {}. It may have been written by another \
                     version of loom, in which case it has to be deleted.",
                    fs_path.display(),
                    err
                )
            })),
        }
    }

    pub(crate) fn store_execution_path(path: &crate::rt::Path, fs_path: &Path, model: &str) {
        let serialized = serde_json::to_string(&Store { model, path }).unwrap();

        if let Some(dir) = fs_path.parent() {
            fs::create_dir_all(dir).unwrap();
        }

        let mut file = File::create(fs_path).unwrap();
        file.write_all(serialized.as_bytes()).unwrap();
    }
}

#[cfg(not(feature = "checkpoint"))]
mod checkpoint {
    use std::path::{Path, PathBuf};

    pub(crate) fn file_for(path: &Path, _model: &str) -> PathBuf {
        path.to_path_buf()
    }

    pub(crate) fn load_execution_path(_fs_path: &Path, _model: &str) -> Option<crate::rt::Path> {
        panic!("not compiled with `checkpoint` feature")
    }

    pub(crate) fn store_execution_path(_path: &crate::rt::Path, _fs_path: &Path, _model: &str) {
        panic!("not compiled with `checkpoint` feature")
    }
}
