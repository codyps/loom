#![deny(warnings, rust_2018_idioms)]

//! Exploring a model with several workers has to find what a single one
//! finds.

use loom::sync::atomic::AtomicUsize;
use loom::sync::atomic::Ordering::SeqCst;
use loom::sync::mpsc::channel;
use loom::sync::{Arc, Mutex};
use loom::thread;

use std::collections::BTreeSet;
use std::sync::atomic::AtomicUsize as StdAtomicUsize;

/// Runs `f` under loom and returns the outcomes and the number of executions.
fn explore<T, F>(workers: usize, bound: Option<usize>, f: F) -> (BTreeSet<T>, usize)
where
    T: Ord + Send + 'static,
    F: Fn() -> T + Sync + Send + 'static,
{
    let seen = std::sync::Arc::new(std::sync::Mutex::new(BTreeSet::new()));
    let count = std::sync::Arc::new(StdAtomicUsize::new(0));

    {
        let seen = seen.clone();
        let count = count.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = bound;
        builder.workers = workers;

        builder.check(move || {
            count.fetch_add(1, SeqCst);
            let outcome = f();
            seen.lock().unwrap().insert(outcome);
        });
    }

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    (seen, count.load(SeqCst))
}

/// Checks that `f` has the same outcomes however many workers explore it,
/// with and without a preemption bound, and returns them.
fn same_outcomes<T, F>(f: F) -> BTreeSet<T>
where
    T: Ord + Send + std::fmt::Debug + 'static,
    F: Fn() -> T + Sync + Send + Copy + 'static,
{
    for bound in [Some(1), Some(2)] {
        let (one, _) = explore(1, bound, f);

        for workers in [2, 4] {
            let (several, _) = explore(workers, bound, f);
            assert_eq!(one, several, "{} workers, bound {:?}", workers, bound);
        }
    }

    let (one, count) = explore(1, None, f);

    for workers in [2, 3, 8] {
        let (several, several_count) = explore(workers, None, f);

        assert_eq!(one, several, "{} workers", workers);

        // The workers do not repeat each other's work. Which executions are
        // found to be repeats only on the way differs a little.
        assert!(
            several_count <= count + count / 4 + 8,
            "{} executions with {} workers, {} with one",
            several_count,
            workers,
            count
        );
    }

    one
}

#[test]
fn atomics() {
    let seen = same_outcomes(|| {
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let x = x.clone();
                thread::spawn(move || (x.fetch_add(1, SeqCst), x.fetch_add(1, SeqCst)))
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    // 6! / (2! 2! 2!) orders, each of which gives different results.
    assert_eq!(seen.len(), 90);
}

#[test]
fn mutex_and_arc_handles() {
    let seen = same_outcomes(|| {
        let log = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..3)
            .map(|i| {
                let log = log.clone();
                thread::spawn(move || {
                    log.lock().unwrap().push(i);
                    log.lock().unwrap().push(i);
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let log = log.lock().unwrap().clone();
        log
    });

    assert_eq!(seen.len(), 90);
}

#[test]
fn channel_and_relaxed_loads() {
    use loom::sync::atomic::Ordering::Relaxed;

    let seen = same_outcomes(|| {
        let (tx, rx) = channel();
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (1..=2)
            .map(|i| {
                let tx = tx.clone();
                let x = x.clone();

                thread::spawn(move || {
                    x.store(i, Relaxed);
                    tx.send((i, x.load(Relaxed))).unwrap();
                })
            })
            .collect();

        drop(tx);

        let mut received = vec![];

        while let Ok(message) = rx.recv() {
            received.push(message);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        received
    });

    // Either message first, and each thread reads its own store or the
    // other's.
    assert!(seen.contains(&vec![(1, 1), (2, 2)]), "{:?}", seen);
    assert!(seen.contains(&vec![(2, 1), (1, 1)]), "{:?}", seen);
    assert!(seen.len() >= 6, "{:?}", seen);
}

/// A failure in any worker is the failure of the check.
#[test]
#[should_panic(expected = "lost an increment")]
fn failure_is_reported() {
    let mut builder = loom::model::Builder::new();
    builder.workers = 4;

    builder.check(|| {
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let x = x.clone();
                thread::spawn(move || {
                    let value = x.load(SeqCst);
                    x.store(value + 1, SeqCst);
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(x.load(SeqCst), 3, "lost an increment");
    });
}

/// A deadlock is found by whichever worker gets to it.
#[test]
#[should_panic(expected = "deadlock")]
fn deadlock_is_reported() {
    let mut builder = loom::model::Builder::new();
    builder.workers = 4;

    builder.check(|| {
        let a = std::rc::Rc::new(Mutex::new(1));
        let b = std::rc::Rc::new(Mutex::new(2));

        let first = {
            let (a, b) = (a.clone(), b.clone());
            thread::spawn(move || {
                let a = a.lock().unwrap();
                let b = b.lock().unwrap();
                assert_eq!(*a + *b, 3);
            })
        };
        let second = thread::spawn(move || {
            let b = b.lock().unwrap();
            let a = a.lock().unwrap();
            assert_eq!(*a + *b, 3);
        });

        first.join().unwrap();
        second.join().unwrap();
    });
}

/// The limit on the number of executions holds for all workers together.
#[test]
fn max_permutations_stops_every_worker() {
    let count = std::sync::Arc::new(StdAtomicUsize::new(0));

    {
        let count = count.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = None;
        builder.workers = 4;
        builder.checkpoint_interval = 10;
        builder.max_permutations = Some(50);

        builder.check(move || {
            count.fetch_add(1, SeqCst);

            let x = Arc::new(AtomicUsize::new(0));

            let handles: Vec<_> = (0..3)
                .map(|_| {
                    let x = x.clone();
                    thread::spawn(move || {
                        for _ in 0..3 {
                            x.fetch_add(1, SeqCst);
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        });
    }

    // 1,699 without the limit. A worker finishes the execution it is in.
    let count = count.load(SeqCst);
    assert!((45..60).contains(&count), "{}", count);
}
