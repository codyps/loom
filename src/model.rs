//! Model concurrent programs.

use crate::rt::{self, Execution, Scheduler};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, subscriber};
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_MAX_THREADS: usize = 5;
const DEFAULT_MAX_BRANCHES: usize = 1_000;

/// Configure a model
#[derive(Debug)]
#[non_exhaustive] // Support adding more fields in the future
pub struct Builder {
    /// Max number of threads to check as part of the execution.
    ///
    /// This should be set as low as possible and must be less than
    /// [`MAX_THREADS`](crate::MAX_THREADS).
    pub max_threads: usize,

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
    /// An input-selected execution, including the fallback after input exhaustion,
    /// can starve a thread and exceed `max_branches` even for a correct model.
    /// Reaching this limit does not establish a concurrency bug.
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
        let mut execution = Execution::new(
            self.max_threads,
            self.max_branches,
            self.preemption_bound,
            !self.expect_explicit_explore,
        );
        execution.path.set_input(input);
        execution.log = self.log;
        execution.location = self.location;

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
        let mut i = 1;
        let mut _span = tracing::info_span!("iter", message = i).entered();

        let mut execution = Execution::new(
            self.max_threads,
            self.max_branches,
            self.preemption_bound,
            !self.expect_explicit_explore,
        );
        let mut scheduler = Scheduler::new(self.max_threads);

        // Identifies the model a checkpoint belongs to.
        let model = std::any::type_name::<F>();

        let checkpoint_file = self
            .checkpoint_file
            .as_ref()
            .map(|path| checkpoint::file_for(path, model));

        if let Some(ref path) = checkpoint_file {
            if path.exists() {
                if let Some(checkpoint) = checkpoint::load_execution_path(path, model) {
                    execution.path = checkpoint;
                    execution.path.set_max_branches(self.max_branches);
                }
            }
        }

        execution.log = self.log;
        execution.location = self.location;
        execution.path.set_sleep_sets(self.sleep_sets);

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
            if let Some(next) = execution.step() {
                execution = next;
            } else {
                info!(parent: None, "Completed in {} iterations", i - 1);
                return;
            }
        }
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
        f();

        // if `f()` didn't panic, then we terminate the
        // main thread by dropping the guard ourselves.
        drop(panic_guard);
    });

    execution.check_for_leaks();
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
            Err(_) => Some(serde_json::from_str(&contents).unwrap()),
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
