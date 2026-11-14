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
   shares the same permit and waiters.
 - `lock_api` feature: mock `RawMutex` / `RawRwLock` and the `Mutex`,
   `RwLock` and guard aliases built on them, for code using `lock_api` or
   `parking_lot` (upstream PR 161, ported; uses `std::sync::OnceLock` rather
   than a `once_cell` dependency).
 - `loom::boxed::Box`, a leak-tracked `Box` (upstream PR 141, ported). Fixes
   the gaps its author listed: zero-sized values, drop-check marker, `Send` /
   `Sync` / `Unpin` for unsized contents. `Box::into_inner` replaces `*boxed`.
 - `model::Builder::check_with_input`: run one execution whose scheduling,
   atomic read and spurious wakeup choices come from a byte string, for
   driving loom from a fuzzer or replaying an execution (upstream PR 427).
 - `model::Builder::stack_size` and `LOOM_STACK_SIZE`: default stack size for
   all threads, including the main one (upstream PR 412).
 - `thread::JoinHandle::is_finished` (upstream PR 411, fixed: see below).
 - `LocalKey` convenience methods: `set` / `get` / `take` / `replace` for
   `Cell`, and `with_borrow` / `with_borrow_mut` / `set` / `take` / `replace`
   for `RefCell` (upstream PR 430, extended to `std::cell::Cell` and
   `RefCell`; the proposal only covered `loom::cell::Cell`).
 - `Arc::into_inner` (upstream PR 354).
 - `Condvar::wait_while` (upstream PR 167).
 - `UnsafeCell::as_mut`, a safe tracked `&mut` access (upstream PR 379).
 - `AtomicPtr::fetch_ptr_add`, `fetch_ptr_sub`, `fetch_byte_add`,
   `fetch_byte_sub`, `fetch_and`, `fetch_or`, `fetch_xor` (upstream PR 405,
   reimplemented without the Rust 1.75 / 1.84 pointer methods so the MSRV does
   not move).

### Fixed

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

 - Load buffering is not modeled (part of upstream issue 6).
 - Requests that cannot be met by a mock that carries tracking state:
   `repr(transparent)` atomics and cells (51, 317), `const fn new` (170),
   `as_ptr` / `raw_get` / `data_ptr` (298, 276, 234).
 - Design questions and feature requests left alone: catching thread panics
   (417, documented instead), spurious condvar wakeups (1), `MaybeUninit`
   (299), raising `MAX_THREADS` (157), random exploration (11), dropping the
   `generator` dependency (388), and the reports that depend on external code
   that could not be run here (301, 362, 398).

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
   something else first; `UnsafeCell` drop and cross-execution reuse checks.
 - The `block_on` waker test documents the `will_wake` contract but cannot
   fail on the old code in practice: the old vtable was only duplicated under
   particular inlining.
 - The ignored tests are still ignored and still fail: `load_buffering`,
   `relaxed`, `test_seq_cst` and `compare_and_swap_reads_old_values`. The
   atomics fix above does not address them.
 - `Box`: zero-sized values are not leak tracked, and unsized boxes can only
   be made with `Box::from_raw`.
 - `lock_api` locks, like every loom object, cannot be kept in a `static`
   across executions.
 - `Notify::wait` is still limited to one blocked thread at a time.
 - `Condvar` waits never end spuriously, and `wait_timeout` never times out.
 - Scoped threads: as elsewhere in loom, a panicking thread fails the model
   rather than being reported through `join`.
 - `check_with_input`: when the input runs out the first outcome is chosen,
   which can starve a thread and hit `max_branches` on a correct model. The
   author asked upstream whether a different fallback is wanted; unanswered.

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
