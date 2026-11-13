#![deny(warnings, rust_2018_idioms)]

//! Tests that loom explores every outcome a model can have, by collecting
//! the outcomes over all executions and comparing them with what is possible.

use loom::sync::atomic::Ordering::{Relaxed, SeqCst};
use loom::sync::atomic::{AtomicBool, AtomicUsize};
use loom::sync::Arc;
use loom::thread;

use std::collections::BTreeSet;
use std::sync::Mutex;

/// Runs `f` under loom and returns the set of values it returned.
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

/// A load that is irrelevant to the outcome must not hide the race between
/// the main thread's load and the spawned thread's store.
#[test]
fn irrelevant_load_before_store() {
    let seen = outcomes(|| {
        let a1 = Arc::new(AtomicBool::new(false));
        let a2 = a1.clone();

        let th = thread::spawn(move || {
            let _irrelevant = a1.load(Relaxed);
            a1.store(true, Relaxed);
        });

        let observed = a2.load(Relaxed);

        th.join().unwrap();

        observed
    });

    assert_eq!(seen, BTreeSet::from([false, true]));
}

/// Final values of `x` when each of `threads` threads runs
/// `v = x.load(); x.store(v * 10 + id)`.
fn load_then_store(threads: usize) -> BTreeSet<usize> {
    outcomes(move || {
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (1..=threads)
            .map(|id| {
                let x = x.clone();
                thread::spawn(move || {
                    let v = x.load(Relaxed);
                    x.store(v * 10 + id, Relaxed);
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        x.load(Relaxed)
    })
}

/// The final values reachable by interleaving the threads of
/// `load_then_store` one step at a time.
fn load_then_store_interleavings(threads: usize) -> BTreeSet<usize> {
    // Per thread: `None` before its load, `Some(v)` after it, done after
    // its store.
    fn step(
        x: usize,
        loaded: &mut Vec<Option<usize>>,
        done: &mut Vec<bool>,
        out: &mut BTreeSet<usize>,
    ) {
        if done.iter().all(|done| *done) {
            out.insert(x);
            return;
        }

        for id in 0..done.len() {
            if done[id] {
                continue;
            }

            match loaded[id] {
                None => {
                    loaded[id] = Some(x);
                    step(x, loaded, done, out);
                    loaded[id] = None;
                }
                Some(v) => {
                    done[id] = true;
                    step(v * 10 + id + 1, loaded, done, out);
                    done[id] = false;
                }
            }
        }
    }

    let mut out = BTreeSet::new();
    step(
        0,
        &mut vec![None; threads],
        &mut vec![false; threads],
        &mut out,
    );
    out
}

/// Running one thread to completion before the other starts is a legal
/// execution, in either order.
#[test]
fn load_then_store_two_threads() {
    let seen = load_then_store(2);

    assert_eq!(
        load_then_store_interleavings(2),
        BTreeSet::from([1, 2, 12, 21])
    );
    assert_eq!(seen, BTreeSet::from([1, 2, 12, 21]));
}

#[test]
fn load_then_store_three_threads() {
    let expected = load_then_store_interleavings(3);
    assert_eq!(expected.len(), 15);

    let seen = load_then_store(3);
    let missing: Vec<_> = expected.difference(&seen).collect();
    assert!(missing.is_empty(), "outcomes never explored: {:?}", missing);
}

/// Adding a load whose value is unused must not remove outcomes.
#[test]
fn redundant_load_keeps_outcomes() {
    fn run(with_extra_load: bool) -> BTreeSet<(usize, usize)> {
        outcomes(move || {
            let entry = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
            let entry1 = entry.clone();
            let entry2 = entry.clone();

            let t1 = thread::spawn(move || {
                let (key, value) = &*entry1;

                key.store(1, SeqCst);
                value.store(1, SeqCst);
                value.store(0, SeqCst);

                if with_extra_load {
                    key.load(SeqCst);
                }

                key.store(0, SeqCst);
            });

            let t2 = thread::spawn(move || {
                let (key, value) = &*entry2;

                let value = value.load(SeqCst);
                let key = key.load(SeqCst);
                (value, key)
            });

            t1.join().unwrap();
            t2.join().unwrap()
        })
    }

    let expected = BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1)]);

    assert_eq!(run(false), expected);
    assert_eq!(run(true), expected);
}
