#![deny(warnings, rust_2018_idioms)]

use loom::sync::atomic::AtomicUsize;
use loom::thread;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

#[test]
fn yield_completes() {
    loom::model(|| {
        let inc = Arc::new(AtomicUsize::new(0));

        {
            let inc = inc.clone();
            thread::spawn(move || {
                inc.store(1, Relaxed);
            });
        }

        loop {
            if 1 == inc.load(Relaxed) {
                return;
            }

            loom::thread::yield_now();
        }
    });
}

/// Three threads on a spin lock. The two that are waiting must not keep each
/// other busy while the one that holds the lock is never run: the model has
/// to come to an end, with and without a preemption bound.
#[test]
fn spin_lock_with_three_threads() {
    use loom::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::{Acquire, Release};

    for bound in [None, Some(2)] {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = bound;

        builder.check(|| {
            let locked = Arc::new(AtomicBool::new(false));
            let value = Arc::new(AtomicUsize::new(0));

            let handles: Vec<_> = (0..3)
                .map(|_| {
                    let locked = locked.clone();
                    let value = value.clone();

                    thread::spawn(move || {
                        while locked
                            .compare_exchange(false, true, Acquire, Relaxed)
                            .is_err()
                        {
                            loom::hint::spin_loop();
                        }

                        value.store(value.load(Relaxed) + 1, Relaxed);
                        locked.store(false, Release);
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }

            assert_eq!(value.load(Relaxed), 3);
        });
    }
}

/// A thread that waits for a flag by yielding still gets to see everything
/// the other threads can have done by the time the flag is set, whether or
/// not it had to wait.
#[test]
fn yielding_thread_sees_every_state() {
    use loom::sync::atomic::AtomicBool;
    use std::collections::BTreeSet;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::Mutex;

    let seen = Arc::new(Mutex::new(BTreeSet::new()));

    {
        let seen = seen.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = None;

        builder.check(move || {
            let flag = Arc::new(AtomicBool::new(false));
            let x = Arc::new(AtomicUsize::new(0));
            let y = Arc::new(AtomicUsize::new(0));

            let waiter = {
                let (flag, x, y) = (flag.clone(), x.clone(), y.clone());
                thread::spawn(move || {
                    let mut waited = false;

                    while !flag.load(SeqCst) {
                        waited = true;
                        thread::yield_now();
                    }

                    (waited, x.load(SeqCst), y.load(SeqCst))
                })
            };
            let setter = {
                let (flag, y) = (flag.clone(), y.clone());
                thread::spawn(move || {
                    y.store(1, SeqCst);
                    flag.store(true, SeqCst);
                    y.store(2, SeqCst);
                })
            };
            let other = {
                let x = x.clone();
                thread::spawn(move || {
                    x.store(1, SeqCst);
                    x.store(2, SeqCst);
                })
            };

            let outcome = waiter.join().unwrap();
            setter.join().unwrap();
            other.join().unwrap();

            seen.lock().unwrap().insert(outcome);
        });
    }

    let seen = seen.lock().unwrap();

    // Any progress of the thread that has nothing to do with the flag, and
    // the flag's thread at or past setting it.
    let expected: BTreeSet<_> = [false, true]
        .into_iter()
        .flat_map(|waited| (0..=2).flat_map(move |x| (1..=2).map(move |y| (waited, x, y))))
        .collect();

    assert_eq!(*seen, expected);
}
