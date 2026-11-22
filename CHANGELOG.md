# Unreleased (fork)

Changes in this fork on top of upstream 0.7.3. Each entry names the upstream
pull request it is based on ("upstream PR N"); "reworked" means the idea was
kept but the implementation here differs, for the reason given.

The MSRV is unchanged (1.73, checked with `--all-features`). The whole test
suite passes with no features, `futures`, `lock_api` and all features, in debug
and release, and on the lowest allowed direct dependency versions.

### Added

 - `thread::scope`, `Scope`, `ScopedJoinHandle` and `Builder::spawn_scoped`
   (upstream PR 312, reworked). Built on an internal unchecked spawn instead
   of runtime internals, as asked for in review. The scope waits for every
   thread spawned in it, including ones spawned by other scoped threads, and a
   thread's borrowed result is dropped before the scope ends. This covers the
   unsoundness case raised in review.
 - `loom::sync::Notify` gains the `tokio::sync::Notify` API: `notify_one`,
   `notify_last`, `notify_waiters`, `notified()` and `Notified::enable`
   (upstream PR 415, reworked). The proposed version kept its permit and wake
   flags in `std` atomics that loom cannot see, so races between `notified()`
   and the notify methods were not explored; here all state is behind a
   modeled lock. `notify()` is now `notify_one()`, and the blocking `wait()`
   shares the same permit and waiters. Any number of threads may be blocked
   in `wait()` at once; it used to panic for a second one.
 - `lock_api` feature: mock `RawMutex` / `RawRwLock` and the `Mutex`,
   `RwLock` and guard aliases built on them, for code using `lock_api` or
   `parking_lot` (upstream PR 161, ported; uses `std::sync::OnceLock` rather
   than a `once_cell` dependency).
 - `loom::boxed::Box`, a leak-tracked `Box` (upstream PR 141, ported). Fixes
   the gaps its author listed: zero-sized values, drop-check marker, `Send` /
   `Sync` / `Unpin` for unsized contents. `Box::into_inner` replaces `*boxed`.
   A box of a zero-sized value allocates a byte, which `std`'s does not, so
   that leaking it is reported: its destructor never runs. (A zero-sized
   value given to `from_raw` at an address nothing was allocated at is
   still fine, and not tracked.) Boxed slices and strings can be made from
   a `Vec`, a slice, an array, an iterator, a `&str` or a `String`, and
   cloned, as in `std`; coercing a box to an unsized one cannot be done
   outside `std`, and the documentation shows the raw pointer cast that
   does it for a trait object.
 - `model::Builder::check_with_input`: run one execution whose scheduling,
   atomic read and spurious wakeup choices come from a byte string, for
   driving loom from a fuzzer or replaying an execution (upstream PR 427).
   When the input runs out, the first outcome of each choice is taken. No
   input starves a thread that waits by yielding, as a thread that yields
   is not chosen again before the others have had a turn; the proposal
   listed starvation as a limitation, which that rule (see "Fixed") has
   since removed.
 - The whole RC11 reference corpus is checked in and checked by the test
   suite, all of it in an optimized build. It used to be a subset of 2,067
   programs and is now 20,747: the 17,947 of the first corpus, 2,796 more
   with compare-exchange, longer threads, five and six threads, and more
   stores to one variable than loom keeps by default, and 4 that settle
   disagreements with GenMC.
 - A second reference for the litmus tests: GenMC, built from source. It is
   fast enough to check what herd cannot (herd did not finish 205 programs
   of its own corpus; GenMC does all 20,948 in a minute and a half), and it
   knows locks. `tests/litmus_rc11/reference_genmc.txt` has 2,968 programs
   checked against it, half of them with mutexes, the rest larger than
   what herd gets through. GenMC and herd agree on all but 4 of the 20,747
   programs both have a result for; loom agrees with herd on those. Two of
   the bugs under "Fixed" were found this way.
 - `tests/exhaustive.rs`: 22 small models of everything loom provides
   (mutex, condition variable, channel, `RwLock`, `Notify`, park, `Arc`,
   atomics, yielding, `block_on`, scoped threads), each explored the usual
   way and by a search that tries every thread at every point the
   preemption bound allows (`Builder::exhaustive`, hidden), with and
   without several workers. The outcomes have to be the same. This is the
   reference for what the litmus corpus cannot cover, and it found the
   three bugs listed under "Fixed" for yielding and for sleep sets with a
   preemption bound.
 - A model can have 16 threads, counting the main one, instead of 5
   (`MAX_THREADS`, upstream issue 157), and that is the default for
   `model::Builder::max_threads` as well. The limit is what the bookkeeping
   per thread is sized for; with the comparison of version vectors rewritten
   to go with it, an execution takes about 2% longer than with 5. What
   decides whether a model with many threads can be checked is how much the
   threads have to do with each other: fifteen threads that keep to
   themselves are one execution, eight that take the same lock once are
   40,320.
 - `model::Builder::atomic_history` and `LOOM_ATOMIC_HISTORY`: the number of
   stores loom keeps per atomic, 7 by default as before and now up to 16. A
   load may return a store made long ago if nothing tells the loading
   thread about a newer one; with more such stores than are kept, the
   oldest are not returned. Keeping more makes operations on an atomic with
   that many stores slower (by half with 12 stores and all of them kept),
   which is why it is a setting and not a new default.
 - Load buffering, as an option (`model::Builder::load_buffering`,
   `LOOM_LOAD_BUFFERING=1`; part of upstream issue 6): a `Relaxed` load may
   return a store that comes after it, which the C++ standard allows and
   RC11, loom's model, does not. Loom guesses the value and checks the guess
   when stores are made. The model is explored twice, first the usual way,
   noting the values each atomic is given, then with loads that may also
   guess one of those values. A guess comes true with a store of the value
   by another thread that does not happen after the load, where reading it
   keeps the modification order and the order of `SeqCst` operations
   consistent; an execution in which a guess does not come true is dropped,
   and a panic while one is open is only reported once it has.
   `model::is_settled` tells a model whether the current execution's
   guesses have come true, for what it records outside of loom.

   Checked against herd with RC11 minus its no-thin-air axiom, on the 1,953
   programs of the corpus in which load buffering can be seen
   (`tests/litmus_rc11/reference_lb.txt`, checked by the test suite): no
   outcome that it forbids, every outcome RC11 allows, and 873 of the 1,118
   that only load buffering allows. On the rest of the corpus the mode gives
   RC11's outcomes, apart from 86 larger programs where it adds some; herd
   finishes 46 of them and forbids none of those. The outcomes it does not
   find are left out on purpose: only plain
   `Relaxed` loads guess, and until a guess comes true, the loading thread
   may not access that atomic again nor execute an acquire or `SeqCst`
   fence. Two loads copying 0 back and forth never produce anything else:
   values only come from executions without guesses. The tests
   `load_buffering` and `relaxed`, ignored until now, use it.
 - `model::Builder::check_random`: run executions chosen at random, for a
   model that is too large to explore (upstream issue 11). Every
   scheduling, atomic read and spurious wakeup choice picks one of its
   outcomes with the same probability, from a seed; it runs until
   `max_permutations` or `max_duration` is reached, 10,000 executions if
   neither is set. A failing execution is reported with a seed of its own,
   which `Builder::replay_random` runs again by itself. It proves nothing,
   and what it finds is real: on `local-rcu`, whose largest loom test does
   not finish, it runs some 30,000 executions a second, and finds each of
   the two bugs that the test suite's history has within five executions.
 - `AtomicPtr::load_consume`: a load with what C++ calls
   `memory_order_consume`, which `std` does not have and code gets as
   `crossbeam_utils::atomic::AtomicConsume` does (a `Relaxed` load on Arm,
   `Acquire` elsewhere). The load is a `Relaxed` one. What comes after the
   store it read is each access the thread makes to a loom atomic or
   `UnsafeCell` that it got to by the pointer, and not the thread: what it
   does next is not ordered (a store through the pointer is the exception).
   So code that relies on more than its dependent accesses being ordered
   fails, which it does not when the load is modeled as `Acquire`. What
   depends on the load is a guess, by address and by when an object was
   created, that can be wrong both ways. See the documentation of the
   method.
 - `model::Builder::workers` and `LOOM_WORKERS`: explore the executions of a
   model on several threads, see "Performance" below.
 - `model::Builder::stack_size` and `LOOM_STACK_SIZE`: default stack size for
   all threads, including the main one (upstream PR 412).
 - `thread::JoinHandle::is_finished` (upstream PR 411, fixed: see below).
 - `LocalKey` convenience methods: `set` / `get` / `take` / `replace` for
   `Cell`, and `with_borrow` / `with_borrow_mut` / `set` / `take` / `replace`
   for `RefCell` (upstream PR 430, extended to `std::cell::Cell` and
   `RefCell`; the proposal only covered `loom::cell::Cell`).
 - `Arc::into_inner` (upstream PR 354).
 - `Condvar::wait_while` (upstream PR 167), and `Condvar::wait_timeout_while`.
 - `Condvar::wait_timeout` times out, and `Condvar::wait` can end without a
   notification, as with `std` (upstream issue 1). Both used to wait for a
   notification, so code that handles a timeout was never run, and code
   that waits once without checking again was never caught. A thread that
   is waiting stops at some point at which it has not been notified yet:
   other threads may run first, and notify it, in which case it returns
   as notified. Each thread does this at most once per condition variable
   in an execution, so a loop around a wait ends. Spurious wakeups of
   `wait` can be turned off with `model::Builder::spurious_wakeups` or
   `LOOM_SPURIOUS_WAKEUPS=0`; a timed wait can always time out.
 - `UnsafeCell::as_mut`, a safe tracked `&mut` access (upstream PR 379).
 - `AtomicPtr::fetch_ptr_add`, `fetch_ptr_sub`, `fetch_byte_add`,
   `fetch_byte_sub`, `fetch_and`, `fetch_or`, `fetch_xor` (upstream PR 405,
   reimplemented without the Rust 1.75 / 1.84 pointer methods so the MSRV does
   not move).

### Fixed

 - `SeqCst` fences made what came before one fence happen before what came
   after any fence executed later, in loom's order of execution. C++ does
   not: two `SeqCst` fences are ordered in the order of `SeqCst`
   operations, which restricts what loads around them can return, and is
   not happens-before. A data race between threads whose fences were
   linked only through `Relaxed` accesses (a third thread relaying a flag,
   say) was never reported, in any execution. The order of `SeqCst`
   operations already restricts loads as C++ requires, so the fence no
   longer passes on causality; it stays an acquire and release fence. The
   litmus corpus gives the same outcomes, with debug assertions on every
   program, and a test covers the race (upstream loom has the same issue).
 - `SeqCst` fences and stores that are not kept anymore: an internal error
   ("SeqCst order is cyclic") in a debug build, as soon as an atomic had
   one store more than loom keeps and `SeqCst` fences were involved: a
   thread with a fence and seven stores, and another with a fence and a
   swap, was enough. The store that stands for a forgotten one took over
   the fences after loads of it as fences after loads of its own, and so
   ordered a fence before one load of the forgotten store before a fence
   after another, which nothing requires. Those fences are now kept apart,
   with what has to come before each. In an optimized build the edges were
   dropped instead, and the outcomes were right; with 1, 2 or 3 stores
   kept, 444, 244 and 107 programs of the litmus corpora hit the error, and
   every other program gives the same outcomes as before.
 - Preemption bound with locks: outcomes within the bound were missed. A
   thread that is stopped while it holds a lock lets another one run up to
   where it wants the lock, and that one then stops by itself, which is no
   preemption. The search never tried stopping a thread there: taking a
   lock and releasing it were not operations to reorder, as the one that
   waits cannot go first. With a bound they are now. And a thread that is
   about to take or release a lock, or to do anything else that decides
   which threads can run, is not put to sleep in a bounded search: moving
   such a step changes what a preemption is. Found by comparing with the
   exhaustive search on the GenMC programs with locks: 22 of 2,968 missed
   outcomes with a bound of 1, 2 with a bound of 2; none do now.
 - Sleep sets lost outcomes, for a while. A thread was not tried from an
   earlier point for the sake of its next step if it was asleep there, on
   the grounds that the executions in which that step comes first had been
   explored. They had, but the search gets to what follows from reordering
   two operations by going through the reordering at that point, and other
   parts of the search had been left out in turn because this part was
   expected to cover them. Found by GenMC on a program of three threads
   (`Rxs,Wy1r;Ryr,F;Wx2r`), which had three outcomes instead of four. The
   rule is gone, and with it what it saved: the channel model above went
   from 38 executions back to 90.
 - A lock, or anything else a thread does between two `SeqCst` operations,
   did not count as something done elsewhere, which is what decides whether
   happens-before orders `SeqCst` operations of different threads. Only
   accesses to other atomics did. An outcome that is not allowed was
   possible in a program where one thread takes a lock between a `SeqCst`
   store and a `SeqCst` load of the same atomic. Found by GenMC.
 - `compare_exchange` that fails could not find a value that another
   read-modify-write had already replaced. Nothing may come between the
   store such an operation reads and the one it writes, so no second one
   can read the same store; but a compare-exchange that fails writes
   nothing, and may see a stale value like any load. Found by the litmus
   programs with compare-exchange: 36 of 1,500 missed outcomes.
 - Atomics with more stores than loom keeps (7 by default) could produce
   outcomes that cannot happen. The store given up for a new one was the
   one made first, which can be anywhere in the order of the atomic's
   stores; what was known about the order of the others through it was
   lost, and a thread that had read it could go back to an older value. Now
   the store that is earliest in that order is given up, the one that a
   read-modify-write wrote directly after it takes its place, and what it
   meant for the order of `SeqCst` operations is kept. A load that could
   only return a store that is gone fails with a message saying to raise
   `atomic_history`, instead of returning something else. Found by a
   litmus program with eight stores to one variable that herd needed 16
   minutes for. With `LOOM_ATOMIC_HISTORY=3` on the herd corpus, 2,770
   programs had outcomes that are not allowed. None does now (one of
   20,747 still does with 2 stores kept).
 - Yielding: a thread that yields runs again after the others have taken a
   step, so what it gets to do depends on where its yield falls between
   their steps, whatever those are. The search did not know: a thread that
   yields once and then carries on (which is what a spurious wakeup of
   `Notify::wait` or `block_on` does) was not tried ahead of the other
   threads' earlier steps, and a thread still waiting after a yield was
   taken to be runnable where the search wanted to try it. Both lost
   outcomes. Exploring them costs: three threads on a spin lock take 67,694
   executions instead of 218.
 - Preemption bound with sleep sets: whether a thread could carry on after
   a step was recorded once per scheduling decision, although a step can
   include a load, and end with the thread blocked for one value and
   running for another. Outcomes were lost when the first was taken for
   both.
 - Preemption bound: outcomes within the bound were missed. Once the
   allowed preemptions were used up, no other thread was tried at any later
   point, including the points where the running thread had blocked or
   finished and choosing the next one preempts nobody. The first such choice
   of an execution, typically when the main thread starts to wait for the
   threads it spawned, was counted as a preemption as well. With a bound of
   0, three threads were run in two of their six orders. Of 600 random
   litmus programs, 24 missed outcomes with a bound of 1, 2 with a bound of
   2, and 1 with a bound of 3; now none does, compared with a search that
   tries every thread at every point the bound allows. A bounded search
   therefore takes more executions than it did: it explores what it left
   out.
 - A model with three or more threads that spin on something, such as a
   spin lock, ran out of branches: two of the threads could yield to each
   other forever while the thread they were waiting for was never run. A
   thread that yields is now not run again before every other thread that
   can run has taken a step. This takes the yield at its word, see
   "Yielding" in the crate documentation: a pass through a loop that ends
   in a yield should not do anything that matters.
 - Atomics: a plain store could land between the read and the write of a
   concurrent `swap` / `fetch_*` / successful `compare_exchange`, producing
   outcomes no real execution has, typically spurious failures of try-lock
   flags (upstream PR 391). The two models from upstream PR 121, which failed
   before, are now part of the test suite and pass.
 - `Thread::unpark` on a thread blocked in `join` aborted the model with
   `assertion failed: state.notified`. `unpark` now has std's token semantics
   (upstream PR 418). Additional fix here: a `Condvar::wait` that returns by
   consuming a stale token no longer stays queued, where it would swallow a
   later `notify_one` (a false deadlock).
 - `Mutex::try_lock`, `RwLock::try_read` and `RwLock::try_write` could cause
   false deadlock reports, and a racing `try_lock` was never observed to fail
   (upstream PR 383, reworked). The proposal skipped the branch point when the
   lock was held, which would have stopped loom from exploring the try
   operation; instead, threads about to *try* are no longer treated as
   waiters. This also applies to `future::AtomicWaker`, which uses a try-lock.
 - `RwLock`: a thread holding two read guards released the lock when the first
   was dropped, and panicked on the second (upstream PR 428).
 - Thread locals are dropped before lazy statics, and also when a thread
   panics; a deadlock no longer panics twice (upstream PR 297).
 - A panic no longer turns into an abort when a spawned-but-not-started thread
   owns loom objects (upstream PR 304). The crash scenario from upstream PR 47
   (an atomic used in a destructor after a panic) is now a passing test.
 - `future::block_on`: wakers for the same task could fail `Waker::will_wake`
   because the vtable was not a single static (upstream PR 421).
 - `JoinHandle::is_finished` as proposed read the result without a branch
   point, so loom never explored the "already finished" outcome. It is now a
   modeled operation, with a test that both outcomes are seen.
 - `LocalKey::<Cell<T>>::set` as proposed panicked ("RefCell already
   borrowed") on the first `set` of a fresh key.
 - Found while porting, not from an upstream PR:
   - Reporting a leaked raw allocation (`loom::alloc::alloc`, and so a leaked
     `Box`) aborted the process instead of panicking with "Allocation leaked".
   - Dropping a loom `Arc` (or any other object whose drop is a loom
     operation) while a deadlock panic unwinds aborted the process, hiding the
     deadlock report.

### Changed

 - Checkpoint files written before the thread limit was raised cannot be
   read anymore, and have to be deleted: the panic says so.
 - Creating a thread takes about 3.5 KB more stack in a debug build than it
   did with the lower thread limit. A model that was close to overflowing
   its stack may need `LOOM_STACK_SIZE`.
 - Loom objects used in an execution other than the one they were created in
   now panic with a message saying so, instead of an out-of-bounds or internal
   error, or silently touching an unrelated object (upstream PR 292, ported).
 - Dropping or consuming an `UnsafeCell` while a pointer obtained from it is
   still alive is now a panic (upstream PR 351, reworked). The proposal put a
   `Drop` impl on `UnsafeCell<T>`, which makes the drop checker stricter than
   for `std::cell::UnsafeCell`, required ignoring an existing test, and
   reported false races for cells owned by a `std::sync::Arc`. The check here
   lives on the cell's non-generic runtime state and only looks at live
   pointers, so none of that applies.
 - `Builder::check` installs the `LOOM_LOG` subscriber (upstream PR 413), but
   only when the caller has no tracing subscriber of their own; previously
   `loom::model()` always replaced it.
 - Causality violation panics list the accesses involved even when
   `LOOM_LOCATION` is not set, with a hint to set it (upstream PR 279).
 - Many `trace!` events include the source location (upstream PRs 414 and
   403, which were the same change for atomics).
 - Atomics record their creation location, so a race with the creation is
   reported with one (upstream PR 123).
 - The scheduler asserts that the thread it picked is runnable, so a
   non-deterministic model fails with a clear message (upstream PR 305).
 - `tracing-subscriber` 0.3.20 for RUSTSEC-2025-0055 (upstream PR 390). This
   needed `tracing` 0.1.41 as well, and the CI job for minimal versions now
   uses `-Z direct-minimal-versions`: with transitive minimal versions,
   `sharded-slab` picks a `lazy_static` it cannot compile against.

### Performance

How many executions a model takes decides how large a model can be checked.
Counts below are without a preemption bound.

 - Joining a thread no longer races with that thread's exit. Whether the
   thread has finished by the time it is joined cannot be observed, but both
   orders used to be explored, for every join: three threads that do nothing
   took 26 executions and now take 1. `JoinHandle::is_finished` can observe
   it and is still explored against the exit.
 - Sleep sets: once everything that follows from running a thread at some
   point has been explored, that thread is not run again along the other
   paths from that point until something happens that its next operation
   depends on. An execution in which only such threads can run is cut short.
   Three threads storing to one atomic: 6,225 executions before (414 with the
   join change alone), 7 now, where 6 orders exist. Three threads taking one
   lock twice each: 7,847 before, 95 now, where 90 orders exist. On by
   default; `model::Builder::sleep_sets` or `LOOM_SLEEP_SETS=0` turns it off.
   With a preemption bound, see the next entry.

   Checked by running each model in `tests/sleep_sets.rs` both ways and
   comparing the outcomes, and against the RC11 reference corpus (see
   `tests/litmus_rc11`): all 17,930 programs that finish within the budget
   give exactly the reference outcomes, 2,736 more than finished before.

 - `Arc`: only the drop that frees the value is explored against the other
   drops. Which handle goes last is all that can be observed about the order
   in which handles are dropped (the destructor runs on that thread), so
   that drop is tried in place of each other thread's, and clones and the
   remaining drops are not reordered at all. Three threads storing to an
   atomic they share through `loom::sync::Arc` took 6,225 executions at
   first and 27 with the two changes above; they now take 7, the same as
   with handles loom does not see. `Arc::get_mut` is now explored against
   clones and drops like `strong_count`.
 - Channels: a receive that finds a message and a send, which adds one at
   the other end, are not explored in both orders, and neither is a sender
   going away against anything else, unless it is the last one, which
   disconnects the channel. Each remaining sender gets to be the last, as
   with the handles of an `Arc`. `RwLock`: two threads taking the lock for
   reading are not explored in both orders once one of them has been.
   Three threads sending one message each to a receiver that drains the
   channel: 835 executions before, 90 now (426 instead of 2,112 with a
   preemption bound of 3).
 - Source sets. When an operation is to be tried before an earlier one, a
   thread is added to the scheduling decision of the earlier one. Not if
   there is one already that the wanted executions can start with: a
   thread whose first step since then comes after none of the other
   steps that are to stay where they are. Unlike the rule that was taken
   back (see "Fixed"), this never leaves something out because a thread is
   asleep. Two writers and two readers of two atomics: 1,630 executions to
   1,562; three threads doing three `fetch_add` each: 1,699 to 1,680,
   which is the number of orders there are. Not used with a preemption
   bound.
 - Wakeup trees (`model::Builder::wakeup_trees`, on by default;
   `LOOM_WAKEUP_TREES=0` turns it off). When two operations of an execution
   could have been the other way around, the search no longer adds a thread
   to an earlier scheduling decision and sees where that leads. It adds the
   sequence of steps that gets them the other way around, and follows it:
   the steps taken in between that have nothing to do with the earlier
   operation, and then the later one. The sequences that are to be run
   from a decision are kept as a tree. This is optimal dynamic partial
   order reduction (Abdulla, Aronis, Jonsson and Sagonas, POPL 2014), and
   it is what makes it sound to leave out what a sleeping thread covers,
   which the rule that was taken back did without it.

   Executions that are started and turn out to be repeats were what was
   left to save. Three threads sending to a channel that is drained: 89
   executions to 43; three threads with a handle to a shared value: 59 to
   42; four threads doing three operations each on one atomic, or on one
   lock: 370,430 to 369,600, which is the number of orders there are, and
   a third less time per execution for the lock, as the search no longer
   looks at every thread's next operation at every step. Where an
   operation's relation to another depends on what came before (the last
   handle to be dropped), some repeats remain.

   Used when there is no preemption bound and one worker. With a bound the
   search is a different one, and with several workers a tree would have
   to be shared between the worker that explores it and the ones that add
   to it; both keep to the way described above.
 - Several workers (`LOOM_WORKERS`, `model::Builder::workers`; one by
   default). Each explores a part of the tree of executions with a path of
   its own. A worker that runs out is handed an alternative that another
   one has not got to yet, with a copy of the path that leads there; where
   several workers have a copy of a branch, the first to ask for one of its
   alternatives gets it, and learns which ones were taken before, so that
   sleep sets work across workers as they do within one. The executions
   explored are the same as with one worker, give or take the few that are
   found to be repeats on the way. Four threads doing three `fetch_add`
   each, 373,000 executions: 7.5 s with one worker, 3.8 s with two, 1.9 s
   with four, 0.6 s with sixteen. Not used with a checkpoint file.

   Checked with four workers against the RC11 reference corpus (all 17,935
   programs equal) and against the exhaustive bounded search at bounds 0 to
   3; the whole test suite passes with `LOOM_WORKERS=4`.
 - Coroutines and their stacks are kept from one execution to the next.
   Mapping a stack with its guard page and unwinding the coroutine at the
   end, for every thread of every execution, was most of the time of a small
   model: an execution of a model with three or four threads went from about
   50 to about 30 microseconds.

 - Sleep sets with a preemption bound. A bounded search does not explore
   everything below a point, so having run a thread from there says less. A
   thread now sleeps along another path if moving its step up to that point
   cannot cost a preemption more: choosing it there, and leaving it after
   its step, must be no more of a preemption than the choice the other path
   starts with. With a bound of 3: two writers and two readers of two
   atomics 149,868 executions to 64,795, a condition variable queue 851 to
   353, three threads with mostly private work 114 to 52. With a bound of 1
   there is next to nothing to gain. An exhaustive search of those three
   takes 1,630, 38 and 7, so it is still the one to try first.

   Checked against a search that tries every thread at every point the
   bound allows, on 670 litmus programs at bounds 0 to 3: the same outcomes,
   with and without sleep sets.
 - Atomics: the bookkeeping for the order of `SeqCst` operations took most
   of the time of a model that uses atomics, about ten times what an
   execution of a comparable model with locks takes. Sets of graph nodes
   are kept in place instead of on the heap and iterated without scanning
   for empty bits, and the modification order is computed once per change
   instead of once per question. An execution of four threads doing three
   `fetch_add` each goes from 47 to 19 microseconds (372,939 executions:
   17.7 s to 7.3 s). Later, a check of whether a load or read-modify-write
   is consistent with that order stopped copying the `SeqCst` nodes of
   every store of the atomic, and an atomic stopped setting up room for
   every store it may keep before it has any: the litmus corpus runs about
   5% fewer instructions.
 - Stores to one atomic, other than read-modify-writes, are independent of
   each other. Which of two stores comes first in the atomic is not decided
   by the order in which they are executed, but by happens-before and,
   later, by what loads see: executing them the other way around allows
   nothing new. A load is still dependent with every thread's last store,
   and a store with every thread's last load. Three threads storing to one
   atomic that nothing reads take one execution instead of six; the litmus
   corpus gives the same outcomes in a little less time.
 - `SeqCst` fences are not dependent with anything, neither with every
   `SeqCst` load, store and read-modify-write of every atomic, nor with
   each other. What a fence means follows from happens-before and the
   modification order, not from when it is executed: with the fence first,
   the access (or other fence) is checked against what the fence adds to
   the order of `SeqCst` operations; with it first, the fence adds the same
   when it is executed, and a choice that contradicted it would close a
   cycle, which cannot happen (and is checked in a debug build). Fences
   used to depend on each other because each one passed on what it knew
   to the next, which was wrong (see "Fixed"). The litmus corpus gives the
   same outcomes, also checked with debug assertions on every program, in
   about two thirds of the time. A model with a fence in every read
   section, such as an RCU, gains the most: one of `local-rcu`'s loom
   tests went from over 15 million executions to 410,803, others from
   143,881 to 8,291 and from 112,011 to 8,731.

### Documented

 - Mock `lazy_static` values, and loom objects generally, do not outlive the
   execution they were created in (upstream PR 410, rewritten following the
   review comments: a caveat rather than a `# Safety` section on a safe
   function, plus the general rule in the crate docs).
 - `thread::Builder::stack_size` and the new `Builder::stack_size` are in
   machine words, not bytes, because the value goes straight to the
   `generator` crate. The docs said bytes. Behavior is unchanged.

### Upstream issues

After the pull requests, the open upstream issues were reviewed ("upstream
issue N"). Each fix below is its own commit with tests.

Fixed:

 - Exploration could miss interleavings, or stop after a single execution,
   when one thread's atomic load hid another thread's earlier load from the
   backtracking logic (upstream issues 392, 432, 166). Loom now tracks the last
   load per thread. Without a preemption bound this could make a failing model
   pass.
 - The same kind of gap for `Arc`: `strong_count` racing with another
   thread's clone or drop was only explored in the order loom happened to
   execute them, so a count could never be seen to change. Drops were not
   treated as racing with counts at all, and a thread's own count hid other
   threads' counts from its clone. Channels were checked for the same problem
   and are fine.
 - `compare_exchange_weak` can fail spuriously, as it is allowed to: where
   the exchange would succeed, loom also explores the execution in which it
   fails and returns the current value. Each thread does so at most once per
   atomic in an execution, so retry loops end. Code that tries a weak
   exchange once and treats failure as contention is now caught. It used to
   be identical to `compare_exchange`.
 - Releasing a `Mutex` / `RwLock` is now a scheduling point, so a `try_lock`
   (`try_read`, `try_write`) is explored as landing inside another thread's
   critical section even when the holder does nothing else while holding the
   lock (upstream issue 426). Each guard drop adds a step to the execution
   path, which counts towards `max_branches`.
 - `SeqCst` fences are explored in both orders; a model that is only correct
   for one order used to pass (upstream issue 437).
 - An `UnsafeCell` access racing with another access that is still open was
   missed if the last thing done inside the open access was a `Release` store
   (upstream issue 431).
 - False "Concurrent atomic store and `unsync_load`" report when a successful
   compare-exchange acquired the release that followed the `unsync_load`
   (upstream issue 433).
 - `Condvar` no longer shares the thread's unpark token: a wait does not
   consume a token, and a notification is not one (upstream issue 422). This
   replaces the narrower fix made when merging upstream PR 418.
 - `mpsc`: dropping the last `Sender` ends a blocked `recv()` (upstream issue
   213); sending to a dropped `Receiver` fails instead of being reported as
   "Messages leaked" (upstream issue 139); `try_recv()` is a modeled operation
   whose result varies with the schedule, and reports `Disconnected`.
 - Checkpoints record the model they belong to, and one from a different model
   is ignored; before, any second test run with `LOOM_CHECKPOINT_FILE` set
   could panic in the scheduler (upstream issue 425). Pointing the checkpoint
   path at a directory gives every model its own file (upstream issue 271).
 - Double frees through `loom::alloc::dealloc` are reported as double frees,
   with locations (upstream issue 384).

Memory model, checked against RC11 with the herd7 simulator on about 18,000
generated litmus programs (`tests/litmus_rc11`):

 - `SeqCst` loads, stores and fences are ordered as the memory model
   requires, instead of being treated as `AcqRel` (upstream issues 180, 191).
   Loom keeps a graph of which `SeqCst` operations have to come before which
   others (RC11's `psc` relation) and only lets a load return a store if
   that keeps the graph free of cycles. Store buffering, Dekker / Peterson
   style exclusion and the cppreference `SeqCst` example now pass, without
   outcomes that `SeqCst` allows being lost.
 - Loom allowed outcomes no execution has, in 296 of the programs, some of
   them using only `Relaxed`: observing a store could lose the fact that
   another store comes after it, and a store could end up between the two
   halves of a read-modify-write. It also hit an internal assertion on 4
   programs. With the fixes loom produces no forbidden outcome on the corpus.
 - A read-modify-write can read any store its thread could load, not only
   the newest one loom has executed, and writes directly after it. Loom
   used to miss the executions in which an unrelated, already executed store
   comes after both. A compare-exchange can likewise fail on a value that is
   no longer the newest.
 - With all of the above, loom produces exactly the outcomes RC11 allows for
   every program of the corpus that it explores to completion (about 15,000).

Added:

 - `RwLockWriteGuard::downgrade`, for the `std` style lock and, through
   `RawRwLockDowngrade`, for the `lock_api` one (upstream issue 397).
 - `FromIterator<T>` and `From<Vec<T>>` for `Arc<[T]>` (upstream issue 395).
 - `PoisonError` and `TryLockError` re-exported from `loom::sync` (upstream
   issue 394).
 - `AtomicBool::with_mut`, which every other atomic type already had (see
   upstream issue 154).

Documented:

 - What `AtomicWaker` is a mock of (upstream issue 377).
 - Declaring the `loom` cfg so rustc does not warn about it (upstream issue
   352), and `-C debug-assertions` for release runs (upstream issue 361).
 - Operations hidden from loom can also make it model impossible executions
   (upstream issue 407); loom has no notion of time (upstream issue 193).
 - A panic in a spawned thread fails the model and is never returned by
   `join` (upstream issue 417). Kept as is on purpose: delivering it through
   `join` would silently swallow failed assertions on threads that are never
   joined, which is the common case in a model.

Already resolved by the merged pull requests:

 - Re-run with the reproduction from the issue: 423, 376, 249 / 177, 254,
   152 / 179, 291, 424, 389 and 282 (the last two are now regression tests).
 - Covered by the tests added with the corresponding pull request, not re-run
   from the issue text: 416, 406, 402, 396 / 308, 393, 349, 345 / 363.
 - The "index out of bounds" / "unexpected object" reports (399, 378, 290,
   283, 228) look like the cross-execution reuse that now has its own error
   message, but none of them could be reproduced from the report.

Not addressed:

 - Load buffering is explored only with `Builder::load_buffering`, and only
   in part (part of upstream issue 6, see "Added").
 - Requests that cannot be met by a mock that carries tracking state:
   `repr(transparent)` atomics and cells (51, 317), `const fn new` (170),
   `as_ptr` / `raw_get` / `data_ptr` (298, 276, 234).
 - Design questions and feature requests left alone: catching thread panics
   (417, documented instead), `MaybeUninit` (299), dropping the `generator`
   dependency (388), and the reports that depend on external code that
   could not be run here (301, 362, 398).

### Not merged

 - Upstream PR 184 (tracing to debug non-deterministic tests): written in 2020
   against a very different code base, touches 29 files and conflicts
   throughout. Not ported.
 - Upstream PR 12 (arena allocation for `VersionVec`): a work-in-progress
   against code that no longer exists; its author measured no speedup.
 - Upstream PR 127 (deduplicate `RwLock` code): the functions it merged have
   since diverged, so there is nothing left to deduplicate that way.
 - Upstream PR 92 (remove `init_threads`): already done upstream.
 - Upstream PR 382 (Twitter badge in the README): not relevant to this fork.

### Known limitations

 - Behavior changes that can make an existing model fail where it passed:
   `try_lock` can now fail under contention (it could not before, which was
   wrong); an unpark token is no longer lost when the target blocks on
   something else first; `UnsafeCell` drop and cross-execution reuse checks;
   `Condvar` waits that time out or wake up spuriously.
 - The `block_on` waker test documents the `will_wake` contract but cannot
   fail on the old code in practice: the old vtable was only duplicated under
   particular inlining.
 - Load buffering (`Builder::load_buffering`) finds some of what it allows:
   only plain `Relaxed` loads guess a later store, and an execution is
   dropped if the loading thread accesses the atomic again, or executes an
   acquire or `SeqCst` fence, before the store is made. It explores the
   model twice, is not used with several workers, a checkpoint or
   `check_with_input`, and cannot undo what a dropped execution did outside
   of loom (see `model::is_settled`). Of the tests that were ignored,
   `load_buffering` and `relaxed` use it, `test_seq_cst` passes with the
   `SeqCst` fix, and
   `compare_and_swap_reads_old_values` expected an outcome that no execution
   has (a read-modify-write that reads the initial value cannot come after
   the store it raced with); it now checks the outcomes there are.
 - `Box`: a box cannot be coerced to an unsized one (`Box<[T; N]>` to
   `Box<[T]>`, `Box<T>` to `Box<dyn Trait>`); that needs compiler support.
 - `lock_api` locks, like every loom object, cannot be kept in a `static`
   across executions.
 - With only 2 stores kept per atomic (`atomic_history`), loom gives
   outcomes that RC11 forbids for 3 of the 23,715 litmus programs, such as
   `Rxr,Rys,Xy1r;Wx2s,Wy3s,Wy4s;Wy5r,F,Xx6s`: a store is given up while the
   order of `SeqCst` operations around it is still being settled, and an
   edge that closes a cycle is lost with it. Not with 1, nor with 3 or
   more, the default 7 included, and not with versions of those programs
   that give stores up at the default.
 - `Condvar`: a thread times out, or wakes up spuriously, at most once per
   condition variable in an execution. A model that counts timeouts, or
   only gives up after several, does not get past the first.
 - Scoped threads: as elsewhere in loom, a panicking thread fails the model
   rather than being reported through `join`.

# 0.7.3 (November 7, 2026)

This release bumps the MSRV to 1.73. (#364)

### Added

 - Add `try_update` and `update` to atomic types (#429)

### Changed

 - Remove `pin-utils` dependency (#366)
 - Migrate to Rust 2021 edition (#365)

### Fixed

 - Allow `RwLock` to work with `?Sized` types (#401)
 - Fix build error on 32-bit architectures (#370, #434)

### Documented

 - Document `Notify` limitation (#373)
 - Document what effect hints have on loom (#358)
 - Add `checkpoint` feature flag in docs example (#355)

# 0.7.2

This release bumps the MSRV to 1.65. (#332)

### Added

 - Add `Cell::into_inner` (#341)

### Changed

 - Update generator to 0.8.1 (#338)

### Fixed

 - Disable default features of tracing (#343)
 - Fix typo (#344)
 - Use `core::` instead of `std::` in `lazy_static!` macro (#340)
 - Allow Mutex to work with `?Sized` types (#339)

# 0.7.1 (October 2, 2023)

### Added

- Add `Atomic*::into_inner` (#327)
- Add `get_mut` to `Mutex` and `RwLock` (#322)
- Implement `AsRef` and `Borrow` for `Arc` (#325)

# 0.7.0 (August 4, 2023)

### Added

- `explore()`, `stop_exploring()`, `skip_branch()` enable reducing the
  concurrent state exploration (#323).

# 0.6.1 (July 21, 2023)

### Fixed

- Avoid cancelling generators as it is not a thread-safe operation (#318)

# 0.6.0 (June 17, 2023)

### Changed

- Increase max threads to 5 (#314)

### Added

- Support setting model thread stack size (#311)

### Fixed

- Fix corner case in `RwLock` (#300).

# 0.5.6 (May 19, 2022)

### Added

- cell: add `UnsafeCell::into_inner` for parity with `std` (#272)
- sync: re-enable `Arc::strong_count` (#172)
- sync: implement `Arc::try_unwrap` (#262)
- sync: add `mpsc::Receiver::try_recv` (#262)

### Documented

- show feature flags in docs (#151)
- fix broken RustDoc links (#273)

# 0.5.5 (May 10, 2022)

### Added

- sync: Add `Arc::from_std` without `T: Sized` bound (#226)
- sync: Implement `Debug` for `AtomicPtr` for all `T` (#255)
- logs: Add location tracking for threads and atomic operations (#258)
- logs: Add additional location tracking to `Arc`, `alloc`, and `mpsc` (#265)
- logs: Improve `tracing` configuration for `LOOM_LOG` (#266)
- logs: Add a span for the current model's iteration (#267)

### Documented

- Add note about in-memory representation of atomic types (#253)
- Document `LOOM_LOG` syntax (#257)

### Fixed

- Fix double panic when exceeding the branch limit in `Drop` (#245)
- cell: Allow using `{Mut,Const}Ptr::{deref,with}` when the pointee is `!Sized`
  (#247)
- thread: Fix semantics of `thread::park` after `Thread::unpark` (#250)

# 0.5.4 (December 3, 2021)

### Added

- cell: Add `ConstPtr` and `MutPtr` RAII guards to `UnsafeCell` (#219)

### Changed

- Improve error message when execution state is unavailable (such as when
  running outside of `loom::model`) (#242)

# 0.5.3 (November 23, 2021)

### Added

- thread: Add mock versions of `thread::park` and `Thread::unpark` (#240)

### Changed

- Don't attempt to clean up Mutex when threads are deadlocked (#236)
- Update tracing-subscriber to 0.3 (#238)

# 0.5.2 (October 7, 2021)

### Added

- Add a loom::cell::Cell, which provides a basic wrapper of the loom UnsafeCell (#196)
- Arc counter manipulations (#225)
- Implement `Mutex::into_inner` and `RwLock::into_inner` (#215)
- Implement `Release`, `AcqRel`, and `SeqCst` fences (#220)
- `Arc::as_ptr` added (#230)
- `Arc::pin` added (#224)

### Changed

- Remove implicit `T: Sized` requirement from `UnsafeCell` (#222)
- Update tracing (#227)

# 0.5.1 (July 2, 2021)

### Added

- Add several methods to atomic integer types (#217)

# 0.5.0 (April 12, 2021)

### Breaking

- Bump MSRV to 1.51 (#205)

### Added

- Add `From` implementation to `Mutex` (#131)
- Add `From` implementation to `RwLock` (#209)
- Add `From` implementation to atomic types (#210)
- Add `fetch_update` to atomics (#212)

### Changed

- Move `futures-util` to `dev-dependencies` (#208)
- Update `generator` to 0.7 (#203)

# 0.4.1 (April 1, 2021)

### Added

- Add a `loom::hint` module containing mocked versions of `spin_loop` and `unreachable_unchecked`. (#197)

### Changed

- Switch to non-deprecated `compare_exchange` (#201)

# 0.4.0 (December 3, 2020)

### Added
- `AtomicI8`, `AtomicI16`, `AtomicI32`, `AtomicI64`, and `AtomicIsize` (#189)

### Breaking
- Bump MSRV to `1.45` (#183)

# 0.3.6 (October 8, 2020)

### Added
- `thread::Thread` and `thread::ThreadId` (#175)

# 0.3.5 (July 26, 2020)

### Fixed
- An example in the README failing to compile (#132)

### Changed
- Updated `scoped-tls` to 1.0.0 (#153)

### Added
- `Send` and `Sync` impls for `JoinHandle` (#145)
- `Default` impls for `Mutex`, `RwLock`, and `Condvar` (#138)

# 0.3.4 (May 2, 2020)

### Fixed
- `RwLock` bug with activating threads (#140)

# 0.3.3 (April 28, 2020)

### Fixes
- `RwLock` bug with two writers (#135).

# 0.3.2 (April 13, 2020)

### Fixed
- incorrect location tracking for some atomic types (#122).

### Added
- `lazy_static` support (#125 + #128)
- `mpsc` channel support (#118)

# 0.3.1 (April 8, 2020)

### Fixed
- `UnsafeCell` false negative under some scenarios (#119).

### Added
- `RwLock` support (#88)
- location tracking to atomic types (#114).

# 0.3.0 (March 24, 2020)

### Breaking
- `CausalCell` is renamed `UnsafeCell`
- `Atomic*::get_mut()` is removed in favor of `with` and `with_mut` fns.
- The max threads setting is removed.

### Fixed
- Atomic coherence checking better matches the spec.

### Added
- Models execute much faster
- Loom types are able to perform location tracking for improved error output.

# 0.2.15 (February 25, 2020)

### Fixed
- avoid global happens-before with `SeqCst` ordering (#108).

# 0.2.14 (November 19, 2019)

### Fixed
- internal `async/await` Waker leak (#102).

### Changed
- speed up model runs (#98, #94)

### Added
- `Send` impl for `AtomicWaker`, `Atomic*`
- `AtomicWaker::take_waker` (#103).

# 0.2.13 (November 6, 2019)

### Changed
- update `futures` to 0.3.0 final release (#96).

# 0.2.12 (October 29, 2019)

### Fixed
- thread-local bug when using loom with `--release` (#89).
- omitted state explorations when using SeqCst atomic values (#90).

# 0.2.11 (October 24, 2019)

### Added
- `Mutex::try_lock` (#83).
- stubbed `Condvar::wait_timeout` (#86).

# 0.2.10 (October 15, 2019)

### Added
- `alloc_zeroed` (#77).
- `AtomicPtr::get_mut` (#80).

# 0.2.9 (October 9, 2019)

### Fixed
- `thread_local` initialization & dropping with loom primitives (#74).

### Added
- Basic leak checking (#73).
- `Arc::get_mut` (#74).
- mocked `thread::Builder` (#74).

# 0.2.8 (September 30, 2019)

### Chore
- Update futures-util dependency version (#70).

# 0.2.7 (September 26, 2019)

### Fixed
- `CausalCell` state was updated even when a deferred check was abandoned (#65).
- Add `yield_now` in `AtomicWaker` when entering a potential spin lock due to
  task yielding (#66).

# 0.2.6 (September 25, 2019)

### Changed
- `futures::block_on` polls spuriously (#59).
- mocked types match `std` for `Send` and `Sync` (#61).

### Added
- `fetch_xor` for atomic numbers (#54).
- initial `atomic::fence` support (#57).
- `Notify` primitive for writing external mocked types (#60).
- `thread_local!` macro that works with loom threads (#62).
- API for deferring `CausalCell` causality checks (#62).

# 0.2.5 (September 4, 2019)

### Added
- implement `Default` for atomic types (#48).

# 0.2.4 (August 20, 2019)

### Fixed
- only unblock future thread when notified using waker (#44).

# 0.2.3 (August 17, 2019)

### Fixed
- `CausalCell` failed to detect concurrent immutable/mutable access (#42).

# 0.2.2 (August 14, 2019)

### Fixed
- incorrect causality comparison (#38).
- detect race with CausalCell accessed immediately post spawn (#38).

### Added
- implementation of all atomic numeric types (#30).
- `AtomicBool` (#39).
- `Condvar::notify_all` (#40).

# 0.2.1 (August 10, 2019)

### Chore
- Update futures-util dependency version (#35).

### Added
- `sync::Arc` implementation (#9).

# 0.2.0 (August 7, 2019)

### Added
- `sync::Arc` mock implementation (#14).
- `AtomicU32` (#24).
- `Atomic::unsync_load` - load from an atomic without synchronization (#26).
- thread preemption bounding.

### Changed
- remove scheduler implementation choices -- generator only (#23).
- use `std::future` (#20).

# 0.1.1 (February 19, 2019)

### Added
- `sync::Arc` implementation (#9).

# 0.1.0 (January 8, 2019)

* Initial release
