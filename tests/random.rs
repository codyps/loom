#![deny(warnings, rust_2018_idioms)]

//! `Builder::check_random`: executions chosen at random instead of explored.

use loom::model::Builder;
use loom::sync::atomic::AtomicUsize;
use loom::sync::Arc;
use loom::thread;

use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::{Arc as StdArc, Mutex};

/// Two threads each load a counter and store one more: an update is lost if
/// both load before either stores. Returns what each execution ends with.
fn lost_update(seed: u64, executions: usize) -> Vec<usize> {
    let seen = StdArc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();

    let mut builder = Builder::new();
    builder.max_permutations = Some(executions);

    builder.check_random(seed, move || {
        let counter = Arc::new(AtomicUsize::new(0));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let counter = counter.clone();
                thread::spawn(move || {
                    let value = counter.load(SeqCst);
                    counter.store(value + 1, SeqCst);
                })
            })
            .collect();

        for thread in threads {
            thread.join().unwrap();
        }

        recorded.lock().unwrap().push(counter.load(SeqCst));
    });

    let seen = seen.lock().unwrap().clone();
    seen
}

#[test]
fn runs_the_executions_asked_for() {
    assert_eq!(lost_update(1, 1).len(), 1);
    assert_eq!(lost_update(1, 250).len(), 250);
}

#[test]
fn same_seed_same_executions() {
    assert_eq!(lost_update(7, 200), lost_update(7, 200));
    assert_ne!(lost_update(7, 200), lost_update(8, 200));
}

/// Both outcomes turn up, the one with the lost update included.
#[test]
fn finds_both_outcomes() {
    let seen = lost_update(42, 200);

    assert!(seen.contains(&1), "the lost update was never chosen");
    assert!(seen.contains(&2));
}

/// A failure in one of the executions fails the check.
#[test]
#[should_panic(expected = "an update was lost")]
fn reports_a_failure() {
    let mut builder = Builder::new();
    builder.max_permutations = Some(1_000);

    builder.check_random(3, || {
        let counter = Arc::new(AtomicUsize::new(0));

        let other = {
            let counter = counter.clone();
            thread::spawn(move || {
                let value = counter.load(SeqCst);
                counter.store(value + 1, SeqCst);
            })
        };

        let value = counter.load(SeqCst);
        counter.store(value + 1, SeqCst);

        other.join().unwrap();

        assert_eq!(counter.load(SeqCst), 2, "an update was lost");
    });
}

/// Stops when the time is up, with no limit on the number of executions.
#[test]
fn stops_after_max_duration() {
    let count = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = count.clone();

    let mut builder = Builder::new();
    builder.max_duration = Some(std::time::Duration::from_millis(50));

    let start = std::time::Instant::now();
    builder.check_random(0, move || {
        counted.fetch_add(1, Relaxed);
        thread::spawn(|| {}).join().unwrap();
    });

    assert!(count.load(Relaxed) >= 1);
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

/// Threads that wait for each other by yielding are not starved by random
/// choices: a thread that yields waits for the others to take a step.
#[test]
fn yielding_threads_finish() {
    let mut builder = Builder::new();
    builder.max_permutations = Some(300);

    builder.check_random(11, || {
        let lock = Arc::new(loom::sync::Mutex::new(0));

        let threads: Vec<_> = (0..3)
            .map(|_| {
                let lock = lock.clone();
                thread::spawn(move || loop {
                    if let Ok(mut count) = lock.try_lock() {
                        *count += 1;
                        break;
                    }
                    thread::yield_now();
                })
            })
            .collect();

        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(*lock.lock().unwrap(), 3);
    });
}
