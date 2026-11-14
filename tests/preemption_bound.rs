#![deny(warnings, rust_2018_idioms)]

//! A preemption bound limits how often a thread that could carry on is
//! stopped in favor of another one. It does not limit which thread is chosen
//! when the running one cannot carry on: that is no preemption, and every
//! choice has to be explored however few preemptions are allowed.

use loom::sync::atomic::AtomicUsize;
use loom::sync::atomic::Ordering::SeqCst;
use loom::thread;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// Runs `f` under loom with the given bound and returns the set of values
/// it returned.
fn outcomes<T, F>(bound: usize, f: F) -> BTreeSet<T>
where
    T: Ord + Send + 'static,
    F: Fn() -> T + Sync + Send + 'static,
{
    let seen = Arc::new(Mutex::new(BTreeSet::new()));

    {
        let seen = seen.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(bound);

        builder.check(move || {
            let outcome = f();
            seen.lock().unwrap().insert(outcome);
        });
    }

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    seen
}

/// Without any preemption, each thread runs from start to end. The order in
/// which the threads do so is still open: all six are explored.
#[test]
fn threads_run_in_any_order_without_preemption() {
    let seen = outcomes(0, || {
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (1..=3)
            .map(|i| {
                let x = x.clone();
                thread::spawn(move || x.swap(i, SeqCst))
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    // Each thread reports the thread that ran before it, 0 for none.
    let expected = BTreeSet::from([
        vec![0, 1, 2],
        vec![0, 3, 1],
        vec![2, 0, 1],
        vec![3, 0, 2],
        vec![2, 3, 0],
        vec![3, 1, 0],
    ]);

    assert_eq!(seen, expected);
}

/// The one preemption allowed here is used up early, by stopping the first
/// thread between its two stores. Which of the other two threads runs first
/// after it has finished is not a preemption, and both have to be tried.
#[test]
fn free_choice_after_the_last_preemption() {
    let seen = outcomes(1, || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let first = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(1, SeqCst);
                y.store(1, SeqCst);
            })
        };
        let second = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || (x.load(SeqCst), y.swap(2, SeqCst)))
        };
        let third = {
            let y = y.clone();
            thread::spawn(move || y.swap(3, SeqCst))
        };

        first.join().unwrap();

        (second.join().unwrap(), third.join().unwrap())
    });

    // The second thread runs between the first one's stores (the
    // preemption), and after the first has finished the third goes before
    // ... or after what is left.
    assert!(seen.contains(&((1, 0), 1)), "{:?}", seen);
    assert!(seen.contains(&((1, 0), 2)), "{:?}", seen);
    assert!(seen.contains(&((1, 3), 0)), "{:?}", seen);
}
