#![deny(warnings, rust_2018_idioms)]

use loom::sync::atomic::{AtomicBool, AtomicUsize};
use loom::thread;

use std::collections::HashSet;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::Arc;

loom::lazy_static! {
    static ref A: AtomicUsize = AtomicUsize::new(0);
    static ref NO_LEAK: loom::sync::Arc<usize> = Default::default();
    static ref ARC_WITH_SLOW_CONSTRUCTOR: loom::sync::Arc<usize> = { thread::yield_now(); Default::default() };
}

loom::thread_local! {
    static B: usize = A.load(Relaxed);
}

#[test]
#[should_panic]
fn lazy_static_arc_shutdown() {
    loom::model(|| {
        // note that we are not waiting for this thread,
        // so it may access the static during shutdown,
        // which is not okay.
        thread::spawn(|| {
            assert_eq!(**NO_LEAK, 0);
        });
    });
}

#[test]
fn lazy_static_arc_race() {
    loom::model(|| {
        let jh = thread::spawn(|| {
            assert_eq!(**ARC_WITH_SLOW_CONSTRUCTOR, 0);
        });
        assert_eq!(**ARC_WITH_SLOW_CONSTRUCTOR, 0);

        jh.join().unwrap();
    });
}

#[test]
fn lazy_static_arc_doesnt_leak() {
    loom::model(|| {
        assert_eq!(**NO_LEAK, 0);
    });
}

#[test]
fn legal_load_after_lazy_static() {
    loom::model(|| {
        let t1 = thread::spawn(|| {
            B.try_with(|h| *h).unwrap_or_else(|_| A.load(Relaxed));
        });
        let t2 = thread::spawn(|| {
            B.try_with(|h| *h).unwrap_or_else(|_| A.load(Relaxed));
        });
        t1.join().unwrap();
        t2.join().unwrap();
    });
}

#[test]
#[should_panic]
fn invalid_unsync_load_relaxed() {
    loom::model(|| {
        let a = Arc::new(AtomicUsize::new(0));
        let b = a.clone();

        let thread = thread::spawn(move || {
            unsafe { a.unsync_load() };
        });

        b.store(1, Relaxed);

        thread.join().unwrap();
    });
}

#[test]
#[ignore]
#[should_panic]
fn compare_and_swap_reads_old_values() {
    loom::model(|| {
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));

        let a2 = a.clone();
        let b2 = b.clone();

        let th = thread::spawn(move || {
            a2.store(1, Release);
            b2.compare_and_swap(0, 2, AcqRel);
        });

        b.store(1, Release);
        a.compare_and_swap(0, 2, AcqRel);

        th.join().unwrap();

        let a_val = a.load(Acquire);
        let b_val = b.load(Acquire);

        if a_val == 2 && b_val == 2 {
            panic!();
        }
    });
}

#[test]
fn fetch_add_atomic() {
    let solutions = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let solutions1 = std::sync::Arc::clone(&solutions);

    loom::model(move || {
        let a1 = Arc::new(AtomicUsize::new(0));
        let a2 = a1.clone();

        let th = thread::spawn(move || a2.fetch_add(1, Relaxed));

        let v1 = a1.fetch_add(1, Relaxed);
        let v2 = th.join().unwrap();
        let v3 = a1.load(Relaxed);

        solutions1.lock().unwrap().insert((v1, v2, v3));
    });

    let solutions = solutions.lock().unwrap();

    assert!(solutions.contains(&(0, 1, 2)));
    assert!(solutions.contains(&(1, 0, 2)));
    assert_eq!(solutions.len(), 2);
}

#[test]
fn store_does_not_squeeze_in_rmw() {
    let solutions = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let solutions1 = std::sync::Arc::clone(&solutions);

    loom::model(move || {
        let a1 = Arc::new(AtomicUsize::new(0));
        let a2 = a1.clone();

        let th = thread::spawn(move || {
            a1.store(1, Relaxed);
        });

        let b1 = a2.swap(2, Relaxed);

        a2.store(3, Relaxed);

        let b2 = a2.swap(4, Relaxed);

        th.join().unwrap();

        let b3 = a2.load(Relaxed);

        solutions1.lock().unwrap().insert((b1, b2, b3));
    });

    let solutions = solutions.lock().unwrap();

    // store(1) before swap(2).
    assert!(solutions.contains(&(1, 3, 4)));

    // store(1) after swap(2), before store(3).
    assert!(solutions.contains(&(0, 3, 4)));

    // store(1) after store(3), before swap(4).
    assert!(solutions.contains(&(0, 1, 4)));

    // store(1) after swap(4) (but before join).
    assert!(solutions.contains(&(0, 3, 1)));

    assert_eq!(solutions.len(), 4);
}

#[test]
fn store_concurrent_failed_rmw() {
    let solutions = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let solutions1 = std::sync::Arc::clone(&solutions);

    loom::model(move || {
        let a1 = Arc::new(AtomicUsize::new(0));
        let a2 = a1.clone();

        let th = thread::spawn(move || {
            a1.store(1, Relaxed);
        });

        let b1 = a2.compare_exchange(0, 2, Relaxed, Relaxed);

        th.join().unwrap();

        let b2 = a2.load(Relaxed);

        solutions1.lock().unwrap().insert((b1, b2));
    });

    let solutions = solutions.lock().unwrap();

    // store(1) before compare_exchange(0, 2).
    assert!(solutions.contains(&(Err(1), 1)));

    // store(1) after compare_exchange(0, 2).
    assert!(solutions.contains(&(Ok(0), 1)));

    assert_eq!(solutions.len(), 2, "{:?}", solutions);
}

#[test]
fn unordered_stores() {
    let solutions = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let solutions1 = std::sync::Arc::clone(&solutions);

    loom::model(move || {
        let a1 = Arc::new(AtomicUsize::new(0));
        let a2 = a1.clone();

        let th = thread::spawn(move || {
            a1.store(1, Relaxed);
        });

        a2.store(2, Relaxed);

        th.join().unwrap();

        let b = a2.load(Relaxed);

        solutions1.lock().unwrap().insert(b);
    });

    let solutions = solutions.lock().unwrap();

    assert!(solutions.contains(&1));
    assert!(solutions.contains(&2));

    assert_eq!(solutions.len(), 2);
}

/// A try-lock style flag: a concurrent plain store must not land between the
/// read and the write of the `swap`.
#[test]
fn concurrent_rmw_store() {
    loom::model(move || {
        let flag = Arc::new(AtomicBool::new(false));

        let th = thread::spawn({
            let flag = flag.clone();
            move || flag.store(true, Relaxed) // a.k.a. unlock()
        });

        if flag.swap(false, Relaxed) {
            // a.k.a. if !try_lock { return }
            return;
        }

        th.join().unwrap();

        assert!(flag.load(Relaxed)); // a.k.a. is_locked().
    });
}

#[test]
fn update_atomic() {
    loom::model(|| {
        let a1 = Arc::new(AtomicUsize::new(0));
        let a2 = a1.clone();

        let th = thread::spawn(move || a2.update(Relaxed, Relaxed, |v| v + 1));

        let v1 = a1.update(Relaxed, Relaxed, |v| v + 1);
        let v2 = th.join().unwrap();

        assert_ne!(v1, v2);
        assert_eq!(2, a1.load(Relaxed));
    });
}

/// Like `store_does_not_squeeze_in_rmw`, but the RMW thread has previously
/// observed (with a relaxed load) a store from a thread it is not otherwise
/// synchronized with. Every outcome must be explainable by a single total
/// modification order of the three writes.
#[test]
fn rmw_after_relaxed_load_of_concurrent_store() {
    let solutions = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let solutions1 = std::sync::Arc::clone(&solutions);

    loom::model(move || {
        let a = Arc::new(AtomicUsize::new(0));

        let th1 = thread::spawn({
            let a = a.clone();
            move || a.store(1, Relaxed)
        });
        let th2 = thread::spawn({
            let a = a.clone();
            move || a.store(2, Relaxed)
        });

        let loaded = a.load(Relaxed);
        let swapped = a.swap(3, Relaxed);

        th1.join().unwrap();
        th2.join().unwrap();

        let last = a.load(Relaxed);

        solutions1.lock().unwrap().insert((loaded, swapped, last));
    });

    // (value loaded, value replaced by the swap, final value) for each total
    // order of the writes `1`, `2` and the swap's `3`. The swap replaces its
    // immediate predecessor; the earlier load sees that or something older.
    let mut allowed = HashSet::new();
    for order in [
        [1, 2, 3],
        [2, 1, 3],
        [1, 3, 2],
        [2, 3, 1],
        [3, 1, 2],
        [3, 2, 1],
    ] {
        let swap_at = order.iter().position(|&v| v == 3).unwrap();
        let swapped = if swap_at == 0 { 0 } else { order[swap_at - 1] };
        let last = order[2];

        allowed.insert((0, swapped, last));
        for &loaded in &order[..swap_at] {
            allowed.insert((loaded, swapped, last));
        }
    }

    let solutions = solutions.lock().unwrap();
    let mut illegal: Vec<_> = solutions.difference(&allowed).collect();
    illegal.sort();
    assert!(illegal.is_empty(), "illegal outcomes: {:?}", illegal);

    // Each order of the writes is reachable.
    for last in [1, 2, 3] {
        assert!(solutions.iter().any(|&(_, _, l)| l == last));
    }
    for swapped in [0, 1, 2] {
        assert!(solutions.iter().any(|&(_, s, _)| s == swapped));
    }
}

/// Once a thread's RMW has observed the low bit clear, the only operation
/// that could set it (a compare-exchange from 0) can no longer succeed.
#[test]
fn multiple_compare_exchange() {
    use std::sync::atomic::Ordering::SeqCst;

    loom::model(|| {
        let n1 = Arc::new(AtomicUsize::new(0));
        let n2 = n1.clone();
        let n3 = n1.clone();

        thread::spawn(move || {
            n1.store(2, SeqCst);
        });

        thread::spawn(move || {
            let prev = match n2.compare_exchange(0, 2, SeqCst, SeqCst) {
                Ok(prev) | Err(prev) => prev,
            };

            if (prev & 1) == 0 {
                // We observed 0 or 2, so the compare-exchange below either
                // already failed or will fail: the low bit stays clear.
                assert_eq!(0, n2.load(SeqCst) & 1);
            }
        });

        // This only succeeds in setting the low bit if it observes 0.
        let _ = n3.compare_exchange(0, 1, SeqCst, SeqCst);
    });
}

/// Variant of `multiple_compare_exchange` using `fetch_add`.
#[test]
fn fetch_add_and_compare_exchange() {
    use std::sync::atomic::Ordering::SeqCst;

    loom::model(|| {
        let n1 = Arc::new(AtomicUsize::new(0));
        let n2 = n1.clone();
        let n3 = n1.clone();

        thread::spawn(move || {
            let _ = n1.fetch_add(2, SeqCst);
        });

        thread::spawn(move || {
            let prev = n2.fetch_add(2, SeqCst);
            if (prev & 1) == 0 {
                assert_eq!(0, n2.load(SeqCst) & 1);
            }
        });

        let _ = n3.compare_exchange(0, 1, SeqCst, SeqCst);
        n3.store(0, SeqCst);
    });
}

/// An atomic operation performed by a destructor that runs while the model is
/// being torn down after a panic must not abort the process.
#[test]
#[should_panic(expected = "explicit panic")]
fn atomic_load_on_drop_after_panic() {
    use std::sync::atomic::Ordering::SeqCst;

    struct AtomicLoadOnDrop(AtomicUsize);

    impl Drop for AtomicLoadOnDrop {
        fn drop(&mut self) {
            let _ = self.0.load(SeqCst);
        }
    }

    loom::model(|| {
        let a = AtomicLoadOnDrop(AtomicUsize::new(0));

        // Moving the value into a thread that never gets to run means it is
        // dropped together with the thread, during teardown.
        thread::spawn(move || {
            let _a = a;
        });

        // Panic the parent thread.
        panic!();
    });
}

/// A flag claimed with a compare-exchange, read by its owner without
/// synchronization, then released with a `Release` store. The next owner's
/// successful compare-exchange acquires that release, so its store happens
/// after the previous owner's unsynchronized read.
#[test]
fn unsync_load_then_release_then_successful_cas() {
    loom::model(|| {
        let owned = Arc::new(AtomicBool::new(false));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let owned = owned.clone();
                thread::spawn(move || {
                    if owned
                        .compare_exchange(false, true, Acquire, Relaxed)
                        .is_ok()
                    {
                        let _ = unsafe { owned.unsync_load() };
                        owned.store(false, Release);
                    }
                })
            })
            .collect();

        for thread in threads {
            thread.join().unwrap();
        }
    });
}

/// The same, but nothing orders the unsynchronized read before the other
/// thread's successful compare-exchange: that is still a race.
#[test]
#[should_panic(expected = "Causality violation")]
fn unsync_load_races_with_successful_cas() {
    loom::model(|| {
        let flag = Arc::new(AtomicBool::new(false));

        let th = {
            let flag = flag.clone();
            thread::spawn(move || {
                let _ = flag.compare_exchange(false, true, Acquire, Relaxed);
            })
        };

        let _ = unsafe { flag.unsync_load() };

        th.join().unwrap();
    });
}

/// All threads agree on one modification order per atomic. If a thread sees
/// `LOCK` go `1` then `2`, the swap that wrote `2` came after the store of
/// `1`, so it acquired that store and with it the write of `DATA`.
#[test]
fn modification_order_is_shared_between_threads() {
    use loom::sync::atomic::AtomicU32;

    loom::model(|| {
        loom::lazy_static! {
            static ref LOCK: AtomicU32 = AtomicU32::new(0);
            static ref DATA: AtomicU32 = AtomicU32::new(0);
            static ref OBSERVED: AtomicU32 = AtomicU32::new(21);
        }

        let t1 = thread::spawn(|| {
            DATA.store(42, Relaxed);
            LOCK.store(1, Release);
        });

        let t2 = thread::spawn(|| {
            LOCK.swap(2, AcqRel);
            OBSERVED.store(DATA.load(Relaxed), Release);
        });

        let t3 = thread::spawn(|| {
            let lock1 = LOCK.load(Relaxed);
            let lock2 = LOCK.load(Relaxed);
            let observed = OBSERVED.load(Acquire);

            if lock1 == 1 && lock2 == 2 && observed != 21 {
                assert_eq!(observed, 42);
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();
    });
}

/// A swap either sees the other thread's store, or is followed by it. With a
/// mutex ordering the rest, the two values checked below always agree.
#[test]
fn swap_does_not_lose_concurrent_store() {
    use loom::sync::Mutex;

    struct Pair {
        a: Mutex<bool>,
        b: AtomicBool,
    }

    loom::model(|| {
        let pair = Arc::new(Pair {
            a: Mutex::new(true),
            b: AtomicBool::new(true),
        });

        let t1 = {
            let pair = pair.clone();
            thread::spawn(move || {
                let mut a = pair.a.lock().unwrap();
                pair.b.store(false, Relaxed);
                *a = false;
            })
        };

        let t2 = {
            let pair = pair.clone();
            thread::spawn(move || {
                if !pair.b.swap(true, Relaxed) {
                    return;
                }

                let a = pair.a.lock().unwrap();
                let b = pair.b.load(Relaxed);
                assert!(*a == b);
            })
        };

        t1.join().unwrap();
        t2.join().unwrap();
    });
}

/// Every atomic type offers `with_mut` for exclusive, non-atomic access.
#[test]
fn with_mut_on_all_atomic_types() {
    use loom::sync::atomic::AtomicPtr;

    loom::model(|| {
        let mut flag = AtomicBool::new(false);
        flag.with_mut(|flag| *flag = true);
        assert!(flag.load(Relaxed));

        let mut number = AtomicUsize::new(1);
        number.with_mut(|number| *number += 1);
        assert_eq!(number.load(Relaxed), 2);

        let mut value = 5usize;
        let mut ptr = AtomicPtr::new(std::ptr::null_mut());
        ptr.with_mut(|ptr| *ptr = &mut value);
        assert_eq!(unsafe { *ptr.load(Relaxed) }, 5);
    });
}

/// A read-modify-write writes directly after the store it reads, but that
/// does not have to be the newest store loom has executed. The other thread's
/// store can have been executed already, without the swapping thread knowing
/// of it, and still come last.
#[test]
fn rmw_before_unrelated_earlier_store() {
    let solutions = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let solutions1 = std::sync::Arc::clone(&solutions);

    loom::model(move || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let th = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(1, Relaxed);
                y.store(2, Relaxed);
            })
        };

        // Seeing `y == 2` means the other thread's store to `x` has been
        // executed. Nothing makes this thread aware of it, though.
        let seen = y.load(Relaxed);
        let swapped = x.swap(4, Relaxed);

        th.join().unwrap();

        solutions1
            .lock()
            .unwrap()
            .insert((seen, swapped, x.load(Relaxed)));
    });

    let solutions = solutions.lock().unwrap();

    // The swap replaced the initial value, and the store of 1 came after it.
    assert!(solutions.contains(&(2, 0, 1)), "{:?}", solutions);

    // Whatever the swap replaced, nothing got in between: if it replaced 0,
    // then 1 is the final value, and if it replaced 1, then 4 is.
    for &(_, swapped, last) in solutions.iter() {
        match swapped {
            0 => assert_eq!(last, 1),
            1 => assert_eq!(last, 4),
            other => panic!("swap replaced {}", other),
        }
    }
}

/// Two read-modify-writes never read the same store, so no update is lost,
/// whatever else is going on.
#[test]
fn rmws_do_not_read_the_same_store() {
    loom::model(|| {
        let x = Arc::new(AtomicUsize::new(0));

        let ths: Vec<_> = (0..2)
            .map(|_| {
                let x = x.clone();
                thread::spawn(move || x.fetch_add(1, Relaxed))
            })
            .collect();

        // A plain store that may land anywhere.
        x.store(10, Relaxed);

        let mut read: Vec<_> = ths.into_iter().map(|th| th.join().unwrap()).collect();
        read.sort_unstable();

        let last = x.load(Relaxed);

        match read[..] {
            // Both before the store, both after it, or one on each side.
            [0, 1] => assert!(last == 10 || last == 2),
            [10, 11] => assert_eq!(last, 12),
            [0, 10] => assert_eq!(last, 11),
            _ => panic!("fetch_adds read {:?}", read),
        }
    });
}

/// A compare-exchange can fail on a value that is no longer the newest one,
/// as long as the thread has not seen a newer one.
#[test]
fn compare_exchange_can_fail_on_stale_value() {
    let solutions = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let solutions1 = std::sync::Arc::clone(&solutions);

    loom::model(move || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let th = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(1, Relaxed);
                y.store(1, Relaxed);
            })
        };

        let seen = y.load(Relaxed);
        let result = x.compare_exchange(1, 2, Relaxed, Relaxed);

        th.join().unwrap();

        solutions1.lock().unwrap().insert((seen, result));
    });

    let solutions = solutions.lock().unwrap();

    // `x == 1` has been stored, yet the compare-exchange still finds 0.
    assert!(solutions.contains(&(1, Err(0))), "{:?}", solutions);
    assert!(solutions.contains(&(1, Ok(1))), "{:?}", solutions);
}

/// Runs `f` under loom and returns the set of values it returned.
fn outcomes<T, F>(f: F) -> HashSet<T>
where
    T: std::hash::Hash + Eq + Send + 'static,
    F: Fn() -> T + Sync + Send + 'static,
{
    let seen = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));

    {
        let seen = seen.clone();
        loom::model(move || {
            let outcome = f();
            seen.lock().unwrap().insert(outcome);
        });
    }

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    seen
}

/// A weak compare-exchange can fail even though the value is the expected
/// one. A strong one cannot.
#[test]
fn compare_exchange_weak_fails_spuriously() {
    let weak = outcomes(|| {
        let x = AtomicUsize::new(0);
        (
            x.compare_exchange_weak(0, 1, Relaxed, Relaxed),
            x.load(Relaxed),
        )
    });

    assert_eq!(weak, HashSet::from([(Ok(0), 1), (Err(0), 0)]));

    let strong = outcomes(|| {
        let x = AtomicUsize::new(0);
        (x.compare_exchange(0, 1, Relaxed, Relaxed), x.load(Relaxed))
    });

    assert_eq!(strong, HashSet::from([(Ok(0), 1)]));
}

/// A weak compare-exchange that fails because of the value says so, and is
/// not offered as a spurious failure on top.
#[test]
fn compare_exchange_weak_real_failure() {
    let seen = outcomes(|| {
        let x = AtomicUsize::new(5);
        x.compare_exchange_weak(0, 1, Relaxed, Relaxed)
    });

    assert_eq!(seen, HashSet::from([Err(5)]));
}

/// The usual retry loop ends, and loses no update, with two threads at it.
#[test]
fn compare_exchange_weak_retry_loop() {
    fn increment(x: &AtomicUsize) -> usize {
        let mut attempts = 0;
        let mut current = x.load(Relaxed);

        loop {
            attempts += 1;

            match x.compare_exchange_weak(current, current + 1, Relaxed, Relaxed) {
                Ok(_) => return attempts,
                Err(actual) => current = actual,
            }
        }
    }

    let seen = outcomes(|| {
        let x = Arc::new(AtomicUsize::new(0));

        let th = {
            let x = x.clone();
            thread::spawn(move || increment(&x))
        };

        let mine = increment(&x);
        let theirs = th.join().unwrap();

        assert_eq!(x.load(Relaxed), 2);

        (mine, theirs)
    });

    // Without spurious failures the most a thread needs is two attempts
    // (losing the race once). A spurious failure adds one.
    assert!(seen.contains(&(1, 1)), "{:?}", seen);
    assert!(
        seen.iter().any(|&(mine, theirs)| mine == 3 || theirs == 3),
        "{:?}",
        seen
    );
    assert!(
        seen.iter().all(|&(mine, theirs)| mine <= 3 && theirs <= 3),
        "{:?}",
        seen
    );
}

/// Each thread fails spuriously at most once per atomic, which is what keeps
/// a model with a retry loop finite.
#[test]
fn compare_exchange_weak_fails_once_per_thread() {
    let seen = outcomes(|| {
        let x = AtomicUsize::new(0);

        let first = x.compare_exchange_weak(0, 1, Relaxed, Relaxed).is_ok();
        let current = x.load(Relaxed);
        let second = x
            .compare_exchange_weak(current, current + 1, Relaxed, Relaxed)
            .is_ok();

        (first, second)
    });

    assert_eq!(
        seen,
        HashSet::from([(true, true), (false, true), (true, false)])
    );
}

/// Code that tries a weak compare-exchange once and takes failure to mean
/// that somebody else got there first is wrong. Loom has to say so.
#[test]
#[should_panic(expected = "lock is free, so taking it cannot fail")]
fn compare_exchange_weak_without_retry_is_a_bug() {
    loom::model(|| {
        let locked = AtomicBool::new(false);

        let acquired = locked
            .compare_exchange_weak(false, true, Acquire, Relaxed)
            .is_ok();

        assert!(acquired, "lock is free, so taking it cannot fail");
    });
}

/// The other atomic types have it too.
#[test]
fn compare_exchange_weak_bool_and_ptr() {
    use loom::sync::atomic::AtomicPtr;

    let seen = outcomes(|| {
        let flag = AtomicBool::new(false);
        let ptr = AtomicPtr::new(std::ptr::null_mut::<u8>());
        let mut target = 0u8;

        (
            flag.compare_exchange_weak(false, true, Relaxed, Relaxed)
                .is_ok(),
            ptr.compare_exchange_weak(std::ptr::null_mut(), &mut target, Relaxed, Relaxed)
                .is_ok(),
        )
    });

    assert_eq!(seen.len(), 4, "{:?}", seen);
}
