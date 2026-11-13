#![deny(warnings, rust_2018_idioms)]

//! Litmus tests for `SeqCst` loads and stores.
//!
//! Each test runs a small program under loom, collects the outcomes over all
//! executions, and compares them with what the C++20 / Rust memory model
//! allows. "Forbidden" outcomes guard against loom being too weak (false
//! alarms); "allowed" outcomes guard against it being too strong (missed
//! bugs), which is what modelling every `SeqCst` access as a full fence
//! would be.

use loom::sync::atomic::Ordering::{self, Relaxed, SeqCst};
use loom::sync::atomic::{fence, AtomicUsize};
use loom::sync::Arc;
use loom::thread;

use std::collections::BTreeSet;
use std::sync::Mutex;

fn outcomes<T, F>(f: F) -> BTreeSet<T>
where
    T: Ord + Send + 'static,
    F: Fn() -> T + Sync + Send + 'static,
{
    let seen = std::sync::Arc::new(Mutex::new(BTreeSet::new()));

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

/// Store buffering: each thread writes one variable and reads the other.
fn store_buffering(store: Ordering, load: Ordering) -> BTreeSet<(usize, usize)> {
    outcomes(move || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let t1 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(1, store);
                y.load(load)
            })
        };
        let t2 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                y.store(1, store);
                x.load(load)
            })
        };

        (t1.join().unwrap(), t2.join().unwrap())
    })
}

/// With everything `SeqCst`, at least one thread sees the other's store.
#[test]
fn store_buffering_seq_cst() {
    let seen = store_buffering(SeqCst, SeqCst);
    assert_eq!(seen, BTreeSet::from([(0, 1), (1, 0), (1, 1)]));
}

/// `SeqCst` stores alone do not help: a relaxed load is not part of the
/// `SeqCst` order and may still miss the other store.
#[test]
fn store_buffering_seq_cst_stores_only() {
    let seen = store_buffering(SeqCst, Relaxed);
    assert_eq!(seen, BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1)]));
}

/// ... and neither do `SeqCst` loads alone.
#[test]
fn store_buffering_seq_cst_loads_only() {
    let seen = store_buffering(Relaxed, SeqCst);
    assert_eq!(seen, BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1)]));
}

#[test]
fn store_buffering_relaxed() {
    let seen = store_buffering(Relaxed, Relaxed);
    assert_eq!(seen, BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1)]));
}

/// Dekker / Peterson style mutual exclusion, which relies on exactly the
/// store buffering guarantee.
#[test]
fn dekker_mutual_exclusion() {
    loom::model(|| {
        let data = Arc::new(AtomicUsize::new(0));
        let flag1 = Arc::new(AtomicUsize::new(0));
        let flag2 = Arc::new(AtomicUsize::new(0));

        let th = {
            let (data, flag1, flag2) = (data.clone(), flag1.clone(), flag2.clone());
            thread::spawn(move || {
                flag2.store(1, SeqCst);
                if flag1.load(SeqCst) == 0 {
                    data.fetch_add(1, SeqCst);
                }
            })
        };

        flag1.store(1, SeqCst);
        if flag2.load(SeqCst) == 0 {
            data.fetch_add(1, SeqCst);
        }

        th.join().unwrap();
        assert!(data.load(SeqCst) < 2);
    });
}

/// Independent reads of independent writes: two readers must agree on the
/// order in which the two `SeqCst` stores happened.
fn iriw(store: Ordering, load: Ordering) -> BTreeSet<(usize, usize, usize, usize)> {
    outcomes(move || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let w1 = {
            let x = x.clone();
            thread::spawn(move || x.store(1, store))
        };
        let w2 = {
            let y = y.clone();
            thread::spawn(move || y.store(1, store))
        };
        let r1 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || (x.load(load), y.load(load)))
        };

        let (c, d) = (y.load(load), x.load(load));

        w1.join().unwrap();
        w2.join().unwrap();
        let (a, b) = r1.join().unwrap();

        (a, b, c, d)
    })
}

#[test]
fn iriw_seq_cst() {
    let seen = iriw(SeqCst, SeqCst);

    // One reader sees x written but not y, the other y but not x.
    assert!(!seen.contains(&(1, 0, 1, 0)), "{:?}", seen);

    // Everything else can happen.
    assert_eq!(seen.len(), 15, "{:?}", seen);
}

/// Without `SeqCst`, the readers need not agree.
#[test]
fn iriw_relaxed() {
    let seen = iriw(Relaxed, Relaxed);
    assert_eq!(seen.len(), 16, "{:?}", seen);
}

/// A `SeqCst` load never returns a value older than one an earlier `SeqCst`
/// load (in the `SeqCst` order) returned, across threads.
#[test]
fn seq_cst_loads_do_not_go_back() {
    loom::model(|| {
        let x = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicUsize::new(0));

        let writer = {
            let x = x.clone();
            thread::spawn(move || x.store(1, SeqCst))
        };

        let reader = {
            let (x, seen) = (x.clone(), seen.clone());
            thread::spawn(move || {
                if x.load(SeqCst) == 1 {
                    seen.store(1, SeqCst);
                }
            })
        };

        // If the other reader has told us it saw the store, so do we.
        if seen.load(SeqCst) == 1 {
            assert_eq!(x.load(SeqCst), 1);
        }

        writer.join().unwrap();
        reader.join().unwrap();
    });
}

/// A `SeqCst` load can still return a concurrent relaxed store or the
/// `SeqCst` store, whichever comes later in the modification order.
#[test]
fn seq_cst_load_of_concurrent_relaxed_store() {
    let seen = outcomes(|| {
        let x = Arc::new(AtomicUsize::new(0));

        let t1 = {
            let x = x.clone();
            thread::spawn(move || x.store(1, Relaxed))
        };
        let t2 = {
            let x = x.clone();
            thread::spawn(move || x.store(2, SeqCst))
        };

        t1.join().unwrap();
        t2.join().unwrap();

        x.load(SeqCst)
    });

    assert_eq!(seen, BTreeSet::from([1, 2]));
}

/// `SeqCst` accesses and `SeqCst` fences are part of the same order: store
/// buffering with a `SeqCst` store on one side and a relaxed store followed
/// by a fence on the other.
#[test]
fn store_buffering_fence_and_seq_cst() {
    let seen = outcomes(|| {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let t1 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(1, SeqCst);
                y.load(SeqCst)
            })
        };
        let t2 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                y.store(1, Relaxed);
                fence(SeqCst);
                x.load(Relaxed)
            })
        };

        (t1.join().unwrap(), t2.join().unwrap())
    });

    assert_eq!(seen, BTreeSet::from([(0, 1), (1, 0), (1, 1)]));
}

/// A compare-exchange is a `SeqCst` load and store at once.
#[test]
fn store_buffering_seq_cst_rmw() {
    let seen = outcomes(|| {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let t1 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.swap(1, SeqCst);
                y.load(SeqCst)
            })
        };
        let t2 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                y.fetch_add(1, SeqCst);
                x.load(SeqCst)
            })
        };

        (t1.join().unwrap(), t2.join().unwrap())
    });

    assert_eq!(seen, BTreeSet::from([(0, 1), (1, 0), (1, 1)]));
}
