#![deny(warnings, rust_2018_idioms)]

//! Load buffering: a `Relaxed` load that returns a store which comes after
//! it, see `model::Builder::load_buffering`.

use loom::model::Builder;
use loom::sync::atomic::AtomicUsize;
use loom::sync::Arc;
use loom::thread;

use std::collections::BTreeSet;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::Mutex;

/// What `f` returns in the executions of a model, with load buffering
/// explored or not, leaving out executions that are dropped.
fn outcomes<T, F>(load_buffering: bool, f: F) -> BTreeSet<T>
where
    T: Ord + Send + 'static,
    F: Fn() -> T + Sync + Send + 'static,
{
    let seen = std::sync::Arc::new(Mutex::new(BTreeSet::new()));
    let recorded = seen.clone();

    let mut builder = Builder::new();
    builder.load_buffering = load_buffering;

    builder.check(move || {
        let outcome = f();

        if loom::model::is_settled() {
            recorded.lock().unwrap().insert(outcome);
        }
    });

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    seen
}

/// Each thread loads, then stores: the first stores a constant, the second
/// what it loaded. Each load can read the other thread's store.
fn copy_back() -> (usize, usize) {
    let x = Arc::new(AtomicUsize::new(0));
    let y = Arc::new(AtomicUsize::new(0));

    let first = {
        let (x, y) = (x.clone(), y.clone());
        thread::spawn(move || {
            let a = x.load(Relaxed);
            y.store(1, Relaxed);
            a
        })
    };

    let second = {
        let (x, y) = (x.clone(), y.clone());
        thread::spawn(move || {
            let b = y.load(Relaxed);
            x.store(b, Relaxed);
            b
        })
    };

    (first.join().unwrap(), second.join().unwrap())
}

#[test]
fn load_reads_a_later_store() {
    // Without load buffering, the first thread's load cannot read the 1 that
    // the second copies from the first's later store.
    assert_eq!(outcomes(false, copy_back), [(0, 0), (0, 1)].into());
    assert_eq!(outcomes(true, copy_back), [(0, 0), (0, 1), (1, 1)].into());
}

/// Two threads copy one atomic to the other. Nothing ever stores anything
/// but 0, so 0 is all either can load: a value cannot come out of thin air.
#[test]
fn no_values_out_of_thin_air() {
    let seen = outcomes(true, || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let first = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                let a = x.load(Relaxed);
                y.store(a, Relaxed);
                a
            })
        };

        let second = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                let b = y.load(Relaxed);
                x.store(b, Relaxed);
                b
            })
        };

        (first.join().unwrap(), second.join().unwrap())
    });

    assert_eq!(seen, [(0, 0)].into());
}

/// An assertion on a value read from a later store fails, and the failure
/// is reported: the guess that it rests on came true.
#[test]
#[should_panic(expected = "read the later store")]
fn failure_with_a_guess_is_reported() {
    let mut builder = Builder::new();
    builder.load_buffering = true;

    builder.check(|| {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let first = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                let a = x.load(Relaxed);
                y.store(1, Relaxed);
                // Fails before the other thread has made the store it read.
                assert_ne!(a, 1, "read the later store");
            })
        };

        let second = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || x.store(y.load(Relaxed), Relaxed))
        };

        first.join().unwrap();
        second.join().unwrap();
    });
}

/// A thread guesses the value it stores itself later, which no load of it
/// can read. The execution with that guess does not exist, and the panic in
/// it is not reported.
///
/// The same holds for an assertion that comes before the store a guess
/// needs: had the load read that value, the assertion would have stopped
/// the thread from making the store.
#[test]
fn failure_without_the_store_is_not_reported() {
    let mut builder = Builder::new();
    builder.load_buffering = true;

    builder.check(|| {
        let x = Arc::new(AtomicUsize::new(0));

        let other = thread::spawn(|| {});

        let a = x.load(Relaxed);
        assert_ne!(a, 5, "no execution has this");
        x.store(5, Relaxed);

        other.join().unwrap();
    });
}

/// A store that happens after the load cannot be what it read.
#[test]
fn happens_before_rules_out_the_later_store() {
    let seen = outcomes(true, || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let first = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                let a = x.load(Relaxed);
                y.store(1, Release);
                a
            })
        };

        let second = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                if y.load(Acquire) == 1 {
                    x.store(1, Relaxed);
                }
            })
        };

        second.join().unwrap();
        first.join().unwrap()
    });

    assert_eq!(seen, [0].into());
}
