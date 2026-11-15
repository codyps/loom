#![deny(warnings, rust_2018_idioms)]

//! Tests that loom does not explore more executions than it has to for some
//! very small models.
//!
//! How many executions a model takes is what decides how large a model can
//! be checked at all. These tests pin down the cases in which there is
//! nothing to explore, so that operations which are present in nearly every
//! model (spawning and joining threads, sharing an `Arc`) do not multiply
//! the work.

use loom::sync::atomic::AtomicUsize;
use loom::sync::atomic::Ordering::SeqCst;
use loom::sync::Mutex;
use loom::thread;

use std::sync::atomic::AtomicUsize as StdAtomicUsize;

/// Runs `f` under loom, without a preemption bound, and returns the number of
/// executions it took.
fn executions(f: impl Fn() + Sync + Send + 'static) -> usize {
    let count = std::sync::Arc::new(StdAtomicUsize::new(0));

    {
        let count = count.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = None;
        builder.sleep_sets = true;

        builder.check(move || {
            count.fetch_add(1, SeqCst);
            f();
        });
    }

    count.load(SeqCst)
}

/// Threads that do nothing can only be interleaved one way. In particular it
/// makes no difference whether a thread has finished by the time it is
/// joined.
#[test]
fn idle_threads() {
    let count = executions(|| {
        let handles: Vec<_> = (0..3).map(|_| thread::spawn(|| {})).collect();

        for handle in handles {
            handle.join().unwrap();
        }
    });

    assert_eq!(count, 1);
}

/// Threads that each work on something of their own do not interact.
#[test]
fn independent_threads() {
    let count = executions(|| {
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let atomic = AtomicUsize::new(0);
                let mutex = Mutex::new(0);

                thread::spawn(move || {
                    atomic.store(1, SeqCst);
                    *mutex.lock().unwrap() += 1;
                    atomic.fetch_add(1, SeqCst);
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }
    });

    assert_eq!(count, 1);
}

/// Joining in a different order than the threads were spawned in, or not at
/// all, does not add anything either.
#[test]
fn join_order_does_not_matter() {
    let count = executions(|| {
        let first = thread::spawn(|| {});
        let second = thread::spawn(|| {});
        let _detached = thread::spawn(|| {});

        second.join().unwrap();
        first.join().unwrap();
    });

    assert_eq!(count, 1);
}

/// Scoped threads are joined by the scope, which is no different.
#[test]
fn idle_scoped_threads() {
    let count = executions(|| {
        thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| {});
            }
        });
    });

    assert_eq!(count, 1);
}

/// `is_finished`, unlike `join`, does observe whether the thread is done,
/// and both answers have to be explored.
#[test]
fn is_finished_is_explored() {
    let count = executions(|| {
        let handle = thread::spawn(|| {});
        let _ = handle.is_finished();
        handle.join().unwrap();
    });

    // Two would do; the point is that it is not one, and not many.
    assert!((2..=3).contains(&count), "{} executions", count);
}

/// What a thread does after joining another comes after everything that
/// thread did. There is nothing to reorder.
#[test]
fn operations_after_join_are_ordered() {
    let count = executions(|| {
        let shared = loom::sync::Arc::new(AtomicUsize::new(0));

        let handle = {
            let shared = shared.clone();
            thread::spawn(move || shared.store(1, SeqCst))
        };

        handle.join().unwrap();

        assert_eq!(shared.swap(2, SeqCst), 1);
    });

    assert_eq!(count, 1);
}

/// Three stores to one atomic can be ordered six ways. The search may repeat
/// itself a little on the way, but not by much.
#[test]
fn stores_to_a_shared_atomic() {
    let count = executions(|| {
        let x = std::sync::Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..3)
            .map(|i| {
                let x = x.clone();
                thread::spawn(move || x.store(i, SeqCst))
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }
    });

    assert!((6..=8).contains(&count), "{}", count);
}

/// Three threads taking a lock twice each: 6! / (2! 2! 2!) = 90 orders.
#[test]
fn critical_sections() {
    let count = executions(|| {
        let x = std::sync::Arc::new(Mutex::new(0));

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let x = x.clone();
                thread::spawn(move || {
                    *x.lock().unwrap() += 1;
                    *x.lock().unwrap() += 1;
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(*x.lock().unwrap(), 6);
    });

    assert!((90..=110).contains(&count), "{}", count);
}

/// Handing each thread a handle to a shared value should cost little. The
/// handles are dropped in some order, of which only one thing can be
/// observed: which one goes last.
#[test]
fn shared_arc_handles() {
    use loom::sync::Arc;

    let count = executions(|| {
        let value = Arc::new(0usize);

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let value = value.clone();
                thread::spawn(move || drop(value))
            })
            .collect();

        drop(value);

        for handle in handles {
            handle.join().unwrap();
        }
    });

    // One execution per handle that can be last would be 4.
    assert!((4..=16).contains(&count), "{} executions", count);
}

/// ... and nothing at all if the threads' handles are gone before the last
/// one is.
#[test]
fn arc_handles_outlived_by_owner() {
    use loom::sync::Arc;

    let count = executions(|| {
        let value = Arc::new(0usize);

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let value = value.clone();
                thread::spawn(move || drop(value))
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        drop(value);
    });

    assert_eq!(count, 1);
}

/// A value shared through loom's `Arc` costs about what the same model costs
/// with handles loom does not see.
#[test]
fn stores_through_shared_arc_handles() {
    use loom::sync::Arc;

    let count = executions(|| {
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..3)
            .map(|i| {
                let x = x.clone();
                thread::spawn(move || x.store(i, SeqCst))
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }
    });

    assert!((6..=8).contains(&count), "{}", count);
}

/// With a preemption bound there is less to conclude from what has been
/// explored, but not nothing: threads that mostly keep to themselves are
/// not interleaved every way the bound allows.
#[test]
fn private_work_with_a_preemption_bound() {
    let count = std::sync::Arc::new(StdAtomicUsize::new(0));

    {
        let count = count.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.sleep_sets = true;

        builder.check(move || {
            count.fetch_add(1, SeqCst);

            let shared = std::sync::Arc::new(AtomicUsize::new(0));

            let handles: Vec<_> = (0..3)
                .map(|_| {
                    let shared = shared.clone();
                    thread::spawn(move || {
                        let own = AtomicUsize::new(0);

                        for _ in 0..4 {
                            own.fetch_add(1, SeqCst);
                        }

                        shared.fetch_add(1, SeqCst);
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }

            assert_eq!(shared.load(SeqCst), 3);
        });
    }

    // 114 without sleep sets.
    let count = count.load(SeqCst);
    assert!((6..=80).contains(&count), "{}", count);
}

/// Three threads that each send one message: six orders of the messages.
/// The receiver taking a message that is there does not interact with the
/// next one being sent, and the senders' handles going away matters only
/// for the last of them.
#[test]
fn channel_with_three_senders() {
    use loom::sync::mpsc::channel;

    let count = executions(|| {
        let (tx, rx) = channel();

        let handles: Vec<_> = (0..3)
            .map(|i| {
                let tx = tx.clone();
                thread::spawn(move || tx.send(i).unwrap())
            })
            .collect();

        drop(tx);

        let mut received = 0;

        while rx.recv().is_ok() {
            received += 1;
        }

        assert_eq!(received, 3);

        for handle in handles {
            handle.join().unwrap();
        }
    });

    // 835 with every pair of channel operations taken to be dependent.
    assert!((6..=100).contains(&count), "{}", count);
}

/// Three threads that each drop a handle to a shared value and store to an
/// atomic: six orders of the stores, and any of the handles can be the last
/// to go. Following wakeup trees, the search does not start executions
/// that turn out to be repeats of others, or not many.
#[test]
fn wakeup_trees() {
    fn count(wakeup_trees: bool) -> usize {
        use loom::sync::Arc;

        let count = std::sync::Arc::new(StdAtomicUsize::new(0));

        {
            let count = count.clone();

            let mut builder = loom::model::Builder::new();
            builder.preemption_bound = None;
            builder.sleep_sets = true;
            builder.wakeup_trees = wakeup_trees;
            // Wakeup trees are not used with several workers.
            builder.workers = 1;

            builder.check(move || {
                count.fetch_add(1, SeqCst);

                let x = Arc::new(AtomicUsize::new(0));

                let handles: Vec<_> = (0..3)
                    .map(|i| {
                        let x = x.clone();
                        thread::spawn(move || x.store(i, SeqCst))
                    })
                    .collect();

                drop(x);

                for handle in handles {
                    handle.join().unwrap();
                }
            });
        }

        count.load(SeqCst)
    }

    let (with, without) = (count(true), count(false));

    // 42 and 59.
    assert!(with <= 48 && with < without, "{} and {}", with, without);
}
