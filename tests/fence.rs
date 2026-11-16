#![deny(warnings, rust_2018_idioms)]

use loom::cell::UnsafeCell;
use loom::sync::atomic::{fence, AtomicBool};
use loom::thread;

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use std::sync::Arc;

#[test]
fn fence_sw_base() {
    loom::model(|| {
        let data = Arc::new(UnsafeCell::new(0));
        let flag = Arc::new(AtomicBool::new(false));

        let th = {
            let (data, flag) = (data.clone(), flag.clone());
            thread::spawn(move || {
                data.with_mut(|ptr| unsafe { *ptr = 42 });
                fence(Release);
                flag.store(true, Relaxed);
            })
        };

        if flag.load(Relaxed) {
            fence(Acquire);
            assert_eq!(42, data.with_mut(|ptr| unsafe { *ptr }));
        }
        th.join().unwrap();
    });
}

#[test]
fn fence_sw_collapsed_store() {
    loom::model(|| {
        let data = Arc::new(UnsafeCell::new(0));
        let flag = Arc::new(AtomicBool::new(false));

        let th = {
            let (data, flag) = (data.clone(), flag.clone());
            thread::spawn(move || {
                data.with_mut(|ptr| unsafe { *ptr = 42 });
                flag.store(true, Release);
            })
        };

        if flag.load(Relaxed) {
            fence(Acquire);
            assert_eq!(42, data.with_mut(|ptr| unsafe { *ptr }));
        }
        th.join().unwrap();
    });
}

#[test]
fn fence_sw_collapsed_load() {
    loom::model(|| {
        let data = Arc::new(UnsafeCell::new(0));
        let flag = Arc::new(AtomicBool::new(false));

        let th = {
            let (data, flag) = (data.clone(), flag.clone());
            thread::spawn(move || {
                data.with_mut(|ptr| unsafe { *ptr = 42 });
                fence(Release);
                flag.store(true, Relaxed);
            })
        };

        if flag.load(Acquire) {
            assert_eq!(42, data.with_mut(|ptr| unsafe { *ptr }));
        }
        th.join().unwrap();
    });
}

// SB+fences from the Promising Semantics paper (https://sf.snu.ac.kr/promise-concurrency/)
#[test]
fn sb_fences() {
    loom::model(|| {
        let x = Arc::new(AtomicBool::new(false));
        let y = Arc::new(AtomicBool::new(false));

        let a = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(true, Relaxed);
                fence(SeqCst);
                y.load(Relaxed)
            })
        };

        y.store(true, Relaxed);
        fence(SeqCst);
        let b = x.load(Relaxed);

        if !a.join().unwrap() {
            assert!(b);
        }
    });
}

#[test]
fn fence_hazard_pointer() {
    loom::model(|| {
        let reachable = Arc::new(AtomicBool::new(true));
        let protected = Arc::new(AtomicBool::new(false));
        let allocated = Arc::new(AtomicBool::new(true));

        let th = {
            let (reachable, protected, allocated) =
                (reachable.clone(), protected.clone(), allocated.clone());
            thread::spawn(move || {
                // put in protected list
                protected.store(true, Relaxed);
                fence(SeqCst);
                // validate, then access
                if reachable.load(Relaxed) {
                    assert!(allocated.load(Relaxed));
                }
            })
        };

        // unlink/retire
        reachable.store(false, Relaxed);
        fence(SeqCst);
        // reclaim unprotected
        if !protected.load(Relaxed) {
            allocated.store(false, Relaxed);
        }

        th.join().unwrap();
    });
}

// RWC+syncs from the SCFix paper (https://plv.mpi-sws.org/scfix/)
// The specified behavior was allowed in C/C++11, which later turned out to be too weak.
// C/C++20 and all the implementations of C/C++11 disallow this behavior.
#[test]
fn rwc_syncs() {
    // ... what else would you call them?
    #![allow(clippy::many_single_char_names)]
    loom::model(|| {
        let x = Arc::new(AtomicBool::new(false));
        let y = Arc::new(AtomicBool::new(false));

        let t2 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                let a = x.load(Relaxed);
                fence(SeqCst);
                let b = y.load(Relaxed);
                (a, b)
            })
        };

        let t3 = {
            let x = x.clone();
            thread::spawn(move || {
                y.store(true, Relaxed);
                fence(SeqCst);
                x.load(Relaxed)
            })
        };

        x.store(true, Relaxed);

        let (a, b) = t2.join().unwrap();
        let c = t3.join().unwrap();

        if a && !b && !c {
            panic!();
        }
    });
}

// W+RWC from the SCFix paper (https://plv.mpi-sws.org/scfix/)
// The specified behavior was allowed in C/C++11, which later turned out to be too weak.
// C/C++20 and most of the implementations of C/C++11 disallow this behavior.
#[test]
fn w_rwc() {
    #![allow(clippy::many_single_char_names)]
    loom::model(|| {
        let x = Arc::new(AtomicBool::new(false));
        let y = Arc::new(AtomicBool::new(false));
        let z = Arc::new(AtomicBool::new(false));

        let t2 = {
            let (y, z) = (y.clone(), z.clone());
            thread::spawn(move || {
                let a = z.load(Acquire);
                fence(SeqCst);
                let b = y.load(Relaxed);
                (a, b)
            })
        };

        let t3 = {
            let x = x.clone();
            thread::spawn(move || {
                y.store(true, Relaxed);
                fence(SeqCst);
                x.load(Relaxed)
            })
        };

        x.store(true, Relaxed);
        z.store(true, Release);

        let (a, b) = t2.join().unwrap();
        let c = t3.join().unwrap();

        if a && !b && !c {
            panic!();
        }
    });
}

/// `SeqCst` fences are totally ordered, and loom has to try both orders. Here
/// the reader's fence may come first, in which case it does not see the
/// writer's store to `counter`, although it can later see `flag`.
#[test]
#[should_panic(expected = "counter was not published")]
fn fence_seqcst_order_is_explored() {
    use loom::sync::atomic::AtomicUsize;

    loom::model(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let counter = Arc::new(AtomicUsize::new(0));

        let writer = {
            let (flag, counter) = (flag.clone(), counter.clone());
            thread::spawn(move || {
                counter.store(1, Relaxed);
                fence(SeqCst);
                flag.store(true, Relaxed);
            })
        };

        let reader = {
            let (flag, counter) = (flag.clone(), counter.clone());
            thread::spawn(move || {
                fence(SeqCst);
                if flag.load(Relaxed) {
                    assert!(counter.load(Relaxed) == 1, "counter was not published");
                }
            })
        };

        writer.join().unwrap();
        reader.join().unwrap();
    });
}

/// With the reader's fence placed after it has seen the flag, the fences can
/// only be ordered one way where it matters, and the read is safe.
#[test]
fn fence_seqcst_after_flag() {
    use loom::sync::atomic::AtomicUsize;

    loom::model(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let counter = Arc::new(AtomicUsize::new(0));

        let writer = {
            let (flag, counter) = (flag.clone(), counter.clone());
            thread::spawn(move || {
                counter.store(1, Relaxed);
                fence(SeqCst);
                flag.store(true, Relaxed);
            })
        };

        if flag.load(Relaxed) {
            fence(SeqCst);
            assert_eq!(counter.load(Relaxed), 1);
        }

        writer.join().unwrap();
    });
}

/// What a thread's `swap` returns in each execution of `f`.
fn swap_results(history: usize, f: fn() -> usize) -> std::collections::BTreeSet<usize> {
    let found = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));

    let mut builder = loom::model::Builder::new();
    builder.atomic_history = history;

    let found2 = found.clone();
    builder.check(move || {
        let result = f();
        found2.lock().unwrap().insert(result);
    });

    let found = found.lock().unwrap().clone();
    found
}

/// A store that has to be forgotten to make room for the one a `swap`
/// writes, with `SeqCst` fences in both threads. The new store stands for
/// the forgotten one from then on; a fence before the swap is not ordered
/// before one after a load of the forgotten store, and neither are two
/// fences after loads of it. Loom used to order them, which closed a cycle.
#[test]
fn fences_around_a_forgotten_store() {
    use loom::sync::atomic::AtomicUsize;

    fn model() -> usize {
        let x = Arc::new(AtomicUsize::new(0));

        let fences = thread::spawn(|| {
            fence(SeqCst);
            fence(SeqCst);
        });

        let swapper = {
            let x = x.clone();
            thread::spawn(move || {
                fence(SeqCst);
                x.swap(1, Relaxed)
            })
        };

        fences.join().unwrap();
        swapper.join().unwrap()
    }

    assert_eq!(swap_results(1, model), [0].into());
}

/// The same with more stores than loom keeps by default: one more than
/// that is enough. Each of the stores, and the initial value, can be what
/// the swap reads.
#[test]
fn fences_around_forgotten_stores() {
    use loom::sync::atomic::AtomicUsize;

    fn model() -> usize {
        let x = Arc::new(AtomicUsize::new(0));

        let writer = {
            let x = x.clone();
            thread::spawn(move || {
                fence(SeqCst);
                for i in 1..=7 {
                    x.store(i, Relaxed);
                }
            })
        };

        let swapper = {
            let x = x.clone();
            thread::spawn(move || {
                fence(SeqCst);
                x.swap(8, Relaxed)
            })
        };

        writer.join().unwrap();
        swapper.join().unwrap()
    }

    assert_eq!(swap_results(7, model), (0..=7).collect());
}
