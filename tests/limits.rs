#![deny(warnings, rust_2018_idioms)]

//! The limits of a model: how many threads it can have, and how many stores
//! to an atomic are remembered.

use loom::sync::atomic::AtomicUsize;
use loom::sync::atomic::Ordering::{Relaxed, SeqCst};
use loom::sync::{Arc, Mutex};
use loom::thread;

use std::collections::BTreeSet;

/// As many threads as there can be. They do not interact, so there is one
/// execution to explore.
#[test]
fn max_threads() {
    let executions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    {
        let executions = executions.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = None;

        builder.check(move || {
            executions.fetch_add(1, SeqCst);

            let handles: Vec<_> = (1..loom::MAX_THREADS)
                .map(|i| {
                    thread::spawn(move || {
                        let own = AtomicUsize::new(0);
                        own.store(i, SeqCst);
                        own.load(SeqCst)
                    })
                })
                .collect();

            for (i, handle) in handles.into_iter().enumerate() {
                assert_eq!(handle.join().unwrap(), i + 1);
            }
        });
    }

    assert_eq!(loom::MAX_THREADS, 16);
    assert_eq!(executions.load(SeqCst), 1);
}

/// Eight threads that all take the same lock once: every order, 8! of them,
/// which is more than five threads could show.
#[test]
fn eight_threads_on_a_lock() {
    let orders = std::sync::Arc::new(std::sync::Mutex::new(BTreeSet::new()));

    {
        let orders = orders.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = None;
        builder.sleep_sets = true;
        builder.workers = 4;

        builder.check(move || {
            let log = Arc::new(Mutex::new(Vec::new()));

            let handles: Vec<_> = (0..7u8)
                .map(|i| {
                    let log = log.clone();
                    thread::spawn(move || log.lock().unwrap().push(i))
                })
                .collect();

            log.lock().unwrap().push(7);

            for handle in handles {
                handle.join().unwrap();
            }

            let order = log.lock().unwrap().clone();
            orders.lock().unwrap().insert(order);
        });
    }

    assert_eq!(orders.lock().unwrap().len(), 40_320);
}

#[test]
#[should_panic(expected = "more than 3 threads")]
fn too_many_threads() {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;

    builder.check(|| {
        let handles: Vec<_> = (0..3).map(|_| thread::spawn(|| {})).collect();

        for handle in handles {
            handle.join().unwrap();
        }
    });
}

/// The values a load can return when it runs after ten stores by another
/// thread, with `history` stores kept.
fn late_reads(history: usize) -> BTreeSet<usize> {
    use std::sync::atomic::AtomicUsize as StdAtomicUsize;

    let seen = std::sync::Arc::new(std::sync::Mutex::new(BTreeSet::new()));

    {
        let seen = seen.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = None;
        builder.atomic_history = history;

        builder.check(move || {
            let x = Arc::new(AtomicUsize::new(0));

            // Counts the stores without loom knowing: this tells the test
            // where the load ran, and the loading thread nothing.
            let stored = std::sync::Arc::new(StdAtomicUsize::new(0));

            let writer = {
                let (x, stored) = (x.clone(), stored.clone());
                thread::spawn(move || {
                    for i in 1..=10 {
                        x.store(i, Relaxed);
                        stored.store(i, SeqCst);
                    }
                })
            };

            // Nothing orders the stores before this load: it may return any
            // of them, or the initial value.
            let read = x.load(Relaxed);
            let late = stored.load(SeqCst) == 10;

            writer.join().unwrap();

            if late {
                seen.lock().unwrap().insert(read);
            }
        });
    }

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    seen
}

/// A load may return a store made long ago. How far back loom goes is the
/// number of stores it keeps.
#[test]
fn atomic_history() {
    assert_eq!(late_reads(16), (0..=10).collect());
    assert_eq!(late_reads(11), (0..=10).collect());

    // The default.
    assert_eq!(late_reads(7), (4..=10).collect());
    assert_eq!(late_reads(1), BTreeSet::from([10]));
}

#[test]
#[should_panic(expected = "between 1 and 16")]
fn atomic_history_out_of_range() {
    let mut builder = loom::model::Builder::new();
    builder.atomic_history = 17;

    builder.check(|| {
        let _ = AtomicUsize::new(0);
    });
}
