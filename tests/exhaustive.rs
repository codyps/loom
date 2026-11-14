#![deny(warnings, rust_2018_idioms)]

//! Checks what loom explores against a search that leaves nothing out.
//!
//! Loom runs a thread from a point only if it can tell that this may make a
//! difference. `Builder::exhaustive` turns that off: every thread that can
//! run is tried at every point, as far as the preemption bound allows.
//! Whatever such a search finds, the usual one has to find too, and it
//! cannot find more. That is checked here for small models of everything
//! loom provides, which is the only reference there is for anything but
//! atomics (see `litmus_reference.rs` for those).

use loom::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
use loom::sync::atomic::{AtomicBool, AtomicUsize};
use loom::sync::mpsc::channel;
use loom::sync::{Arc, Condvar, Mutex, Notify, RwLock};
use loom::thread;

use std::collections::BTreeSet;
use std::sync::mpsc::TryRecvError;

/// Runs `f` under loom and returns the set of values it returned.
fn explore<T, F>(exhaustive: bool, bound: Option<usize>, workers: usize, f: F) -> BTreeSet<T>
where
    T: Ord + Send + 'static,
    F: Fn() -> T + Sync + Send + 'static,
{
    let seen = std::sync::Arc::new(std::sync::Mutex::new(BTreeSet::new()));

    {
        let seen = seen.clone();

        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = bound;
        builder.exhaustive = exhaustive;
        builder.workers = workers;

        builder.check(move || {
            let outcome = f();
            seen.lock().unwrap().insert(outcome);
        });
    }

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    seen
}

/// Checks that the usual search finds what the exhaustive one does, with
/// the given preemption bounds, and returns the outcomes for the last one.
fn check<T, F>(bounds: &[Option<usize>], f: F) -> BTreeSet<T>
where
    T: Ord + Send + std::fmt::Debug + 'static,
    F: Fn() -> T + Sync + Send + Copy + 'static,
{
    let mut last = BTreeSet::new();

    for &bound in bounds {
        let expected = explore(true, bound, 1, f);

        assert_eq!(explore(false, bound, 1, f), expected, "bound {:?}", bound);
        assert_eq!(
            explore(false, bound, 3, f),
            expected,
            "bound {:?}, 3 workers",
            bound
        );

        last = expected;
    }

    last
}

const BOUNDED: &[Option<usize>] = &[Some(0), Some(1), Some(2)];
const ALL: &[Option<usize>] = &[Some(0), Some(1), Some(2), None];

#[test]
fn mutex() {
    let seen = check(ALL, || {
        let log = Arc::new(Mutex::new(Vec::new()));

        let other = {
            let log = log.clone();
            thread::spawn(move || {
                log.lock().unwrap().push(1);
                log.lock().unwrap().push(2);
            })
        };

        log.lock().unwrap().push(3);
        log.lock().unwrap().push(4);

        other.join().unwrap();

        let log = log.lock().unwrap().clone();
        log
    });

    // 1 before 2, and 3 before 4.
    assert_eq!(seen.len(), 6);
}

#[test]
fn mutex_three_threads() {
    check(BOUNDED, || {
        let log = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..3)
            .map(|i| {
                let log = log.clone();
                thread::spawn(move || {
                    log.lock().unwrap().push(i);
                    log.lock().unwrap().push(i + 10);
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let log = log.lock().unwrap().clone();
        log
    });
}

#[test]
fn try_lock() {
    check(ALL, || {
        let lock = Arc::new(Mutex::new(0usize));

        let other = {
            let lock = lock.clone();
            thread::spawn(move || match lock.try_lock() {
                Ok(mut guard) => {
                    *guard += 1;
                    true
                }
                Err(_) => false,
            })
        };

        let value = {
            let mut guard = lock.lock().unwrap();
            *guard += 10;
            *guard
        };

        let tried = other.join().unwrap();
        let end = *lock.lock().unwrap();

        (tried, value, end)
    });
}

#[test]
fn condvar() {
    check(BOUNDED, || {
        let state = Arc::new((Mutex::new(Vec::new()), Condvar::new()));

        let producers: Vec<_> = (0..2)
            .map(|i| {
                let state = state.clone();
                thread::spawn(move || {
                    state.0.lock().unwrap().push(i);
                    state.1.notify_one();
                })
            })
            .collect();

        let mut received = vec![];

        while received.len() < 2 {
            let mut queue = state.0.lock().unwrap();

            while queue.is_empty() {
                queue = state.1.wait(queue).unwrap();
            }

            received.append(&mut queue);
        }

        for producer in producers {
            producer.join().unwrap();
        }

        received
    });
}

#[test]
fn channel_with_try_recv() {
    check(BOUNDED, || {
        let (tx, rx) = channel();

        let senders: Vec<_> = (0..2)
            .map(|i| {
                let tx = tx.clone();
                thread::spawn(move || tx.send(i).unwrap())
            })
            .collect();

        drop(tx);

        let mut received = vec![];

        for _ in 0..2 {
            received.push(match rx.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => 8,
                Err(TryRecvError::Disconnected) => 9,
            });
        }

        while let Ok(message) = rx.recv() {
            received.push(message);
        }

        for sender in senders {
            sender.join().unwrap();
        }

        received
    });
}

#[test]
fn channel_receiver_dropped() {
    check(ALL, || {
        let (tx, rx) = channel();

        let sender = thread::spawn(move || (tx.send(1).is_ok(), tx.send(2).is_ok()));

        let first = rx.try_recv().ok();
        drop(rx);

        (first, sender.join().unwrap())
    });
}

#[test]
fn rwlock() {
    check(BOUNDED, || {
        let lock = Arc::new(RwLock::new(0usize));

        let readers: Vec<_> = (0..2)
            .map(|_| {
                let lock = lock.clone();
                thread::spawn(move || {
                    let first = *lock.read().unwrap();
                    let second = lock.try_read().map(|guard| *guard).unwrap_or(9);
                    (first, second)
                })
            })
            .collect();

        *lock.write().unwrap() += 1;
        let wrote_again = lock.try_write().map(|mut guard| *guard += 1).is_ok();

        let read: Vec<_> = readers
            .into_iter()
            .map(|reader| reader.join().unwrap())
            .collect();

        (read, wrote_again)
    });
}

#[test]
fn notify_and_park() {
    check(BOUNDED, || {
        let notify = Arc::new(Notify::new());
        let step = Arc::new(AtomicUsize::new(0));

        let waiter = {
            let (notify, step) = (notify.clone(), step.clone());
            thread::spawn(move || {
                notify.wait();
                step.fetch_add(1, SeqCst)
            })
        };

        let parked = {
            let step = step.clone();
            thread::spawn(move || {
                thread::park();
                step.fetch_add(10, SeqCst)
            })
        };

        let before = step.fetch_add(100, SeqCst);
        notify.notify();
        parked.thread().unpark();

        (before, waiter.join().unwrap(), parked.join().unwrap())
    });
}

#[test]
fn arc_counts_and_get_mut() {
    check(BOUNDED, || {
        let mut value = Arc::new(0usize);

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let value = value.clone();
                thread::spawn(move || Arc::strong_count(&value))
            })
            .collect();

        let unique = Arc::get_mut(&mut value).is_some();
        let count = Arc::strong_count(&value);

        let counts: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        (unique, count, counts)
    });
}

#[test]
fn atomics_with_a_lock() {
    check(BOUNDED, || {
        let ready = Arc::new(AtomicBool::new(false));
        let data = Arc::new(AtomicUsize::new(0));
        let lock = Arc::new(Mutex::new(0usize));

        let writer = {
            let (ready, data, lock) = (ready.clone(), data.clone(), lock.clone());
            thread::spawn(move || {
                data.store(1, Relaxed);
                *lock.lock().unwrap() += 1;
                ready.store(true, Release);
                data.store(2, Relaxed);
            })
        };

        let reader = {
            let (ready, data, lock) = (ready.clone(), data.clone(), lock.clone());
            thread::spawn(move || {
                let ready = ready.load(Acquire);
                let locked = *lock.lock().unwrap();
                (ready, locked, data.load(Relaxed))
            })
        };

        writer.join().unwrap();
        reader.join().unwrap()
    });
}

#[test]
fn compare_exchange() {
    check(BOUNDED, || {
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (1..=2)
            .map(|i| {
                let x = x.clone();
                thread::spawn(move || {
                    let strong = x.compare_exchange(0, i, SeqCst, Relaxed);
                    let weak = x.compare_exchange_weak(i, i + 10, AcqRel, Relaxed);
                    (strong, weak)
                })
            })
            .collect();

        let seen = x.load(Relaxed);

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        (seen, results, x.load(SeqCst))
    });
}

#[test]
fn yield_until_flag() {
    check(BOUNDED, || {
        let flag = Arc::new(AtomicBool::new(false));
        let value = Arc::new(AtomicUsize::new(0));

        let setter = {
            let (flag, value) = (flag.clone(), value.clone());
            thread::spawn(move || {
                value.store(1, SeqCst);
                flag.store(true, SeqCst);
                value.store(2, SeqCst);
            })
        };

        let other = {
            let value = value.clone();
            thread::spawn(move || value.swap(5, SeqCst))
        };

        while !flag.load(SeqCst) {
            thread::yield_now();
        }

        let seen = value.load(SeqCst);

        setter.join().unwrap();

        (seen, other.join().unwrap())
    });
}

#[test]
fn join_and_is_finished() {
    check(ALL, || {
        let x = Arc::new(AtomicUsize::new(0));

        let handle = {
            let x = x.clone();
            thread::spawn(move || x.swap(1, SeqCst))
        };

        let finished = handle.is_finished();
        let seen = x.swap(2, SeqCst);

        (finished, seen, handle.join().unwrap(), x.load(SeqCst))
    });
}

#[test]
fn scoped_threads() {
    check(BOUNDED, || {
        let x = AtomicUsize::new(0);
        let lock = Mutex::new(Vec::new());

        thread::scope(|scope| {
            for i in 1..=2 {
                let (x, lock) = (&x, &lock);

                scope.spawn(move || {
                    let seen = x.swap(i, SeqCst);
                    lock.lock().unwrap().push(seen);
                });
            }
        });

        let log = lock.lock().unwrap().clone();

        (log, x.load(SeqCst))
    });
}

#[test]
fn spin_lock() {
    check(ALL, || {
        let locked = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));

        let other = {
            let (locked, log) = (locked.clone(), log.clone());
            thread::spawn(move || {
                let mut spins = 0;

                while locked
                    .compare_exchange(false, true, Acquire, Relaxed)
                    .is_err()
                {
                    spins += 1;
                    thread::yield_now();
                }

                log.lock().unwrap().push(1);
                locked.store(false, Release);

                spins.min(1)
            })
        };

        while locked
            .compare_exchange(false, true, Acquire, Relaxed)
            .is_err()
        {
            thread::yield_now();
        }

        log.lock().unwrap().push(2);
        locked.store(false, Release);

        let spun = other.join().unwrap();
        let log = log.lock().unwrap().clone();

        (spun, log)
    });
}

#[test]
fn spin_lock_three_threads() {
    check(BOUNDED, || {
        let locked = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..3)
            .map(|i| {
                let (locked, log) = (locked.clone(), log.clone());
                thread::spawn(move || {
                    while locked
                        .compare_exchange(false, true, Acquire, Relaxed)
                        .is_err()
                    {
                        loom::hint::spin_loop();
                    }

                    log.lock().unwrap().push(i);
                    locked.store(false, Release);
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let log = log.lock().unwrap().clone();
        log
    });
}

#[cfg(feature = "futures")]
#[test]
fn block_on_with_spurious_wakeups() {
    use loom::future::block_on;
    use std::future::poll_fn;
    use std::task::Poll;

    check(BOUNDED, || {
        let gate = Arc::new(AtomicBool::new(false));
        let x = Arc::new(AtomicUsize::new(0));
        let mut polls = 0;

        let other = {
            let x = x.clone();
            thread::spawn(move || x.swap(1, SeqCst))
        };

        let seen = block_on(poll_fn(|cx| {
            if polls == 0 {
                let gate = gate.clone();
                let waker = cx.waker().clone();

                thread::spawn(move || {
                    gate.store(true, Release);
                    waker.wake();
                });
            }

            polls += 1;

            if gate.load(Acquire) {
                Poll::Ready(x.swap(2, SeqCst))
            } else {
                Poll::Pending
            }
        }));

        (polls, seen, other.join().unwrap())
    });
}

/// A thread that blocks or not depending on what a load returned.
#[test]
fn lock_taken_depending_on_a_load() {
    check(BOUNDED, || {
        let flag = Arc::new(AtomicBool::new(false));
        let lock = Arc::new(Mutex::new(Vec::new()));

        let setter = {
            let (flag, lock) = (flag.clone(), lock.clone());
            thread::spawn(move || {
                let mut log = lock.lock().unwrap();
                flag.store(true, Relaxed);
                log.push(1);
            })
        };

        let other = {
            let lock = lock.clone();
            thread::spawn(move || lock.lock().unwrap().push(2))
        };

        let waited = if flag.load(Relaxed) {
            lock.lock().unwrap().push(3);
            true
        } else {
            false
        };

        setter.join().unwrap();
        other.join().unwrap();

        let log = lock.lock().unwrap().clone();

        (waited, log)
    });
}

#[test]
fn condvar_notify_all() {
    check(BOUNDED, || {
        let state = Arc::new((Mutex::new(0usize), Condvar::new()));

        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let state = state.clone();
                thread::spawn(move || {
                    let mut value = state.0.lock().unwrap();

                    while *value == 0 {
                        value = state.1.wait(value).unwrap();
                    }

                    *value += 1;
                    *value
                })
            })
            .collect();

        *state.0.lock().unwrap() = 10;
        state.1.notify_all();

        waiters
            .into_iter()
            .map(|waiter| waiter.join().unwrap())
            .collect::<Vec<_>>()
    });
}

#[test]
fn channel_between_three_threads() {
    check(BOUNDED, || {
        let (tx, rx) = channel();
        let x = Arc::new(AtomicUsize::new(0));

        let sender = {
            let (tx, x) = (tx.clone(), x.clone());
            thread::spawn(move || {
                tx.send(x.swap(1, SeqCst)).unwrap();
                tx.send(x.swap(2, SeqCst)).unwrap();
            })
        };

        let receiver = thread::spawn(move || {
            let mut received = vec![];

            while let Ok(message) = rx.recv() {
                received.push(message);
            }

            received
        });

        let seen = x.swap(3, SeqCst);
        tx.send(7).unwrap();
        drop(tx);

        sender.join().unwrap();

        (seen, receiver.join().unwrap(), x.load(SeqCst))
    });
}

#[test]
fn relaxed_atomics_two_variables() {
    check(ALL, || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let other = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(1, Relaxed);
                let seen = y.load(Relaxed);
                x.store(2, Relaxed);
                seen
            })
        };

        y.store(1, Relaxed);
        let first = x.load(Relaxed);
        let second = x.load(Relaxed);

        (first, second, other.join().unwrap())
    });
}

/// A thread that yields once and then carries on whatever the others did:
/// when it gets to do so depends on where their steps fall around the
/// yield.
#[test]
fn yield_then_act() {
    check(BOUNDED, || {
        let step = Arc::new(AtomicUsize::new(0));

        let yielder = {
            let step = step.clone();
            thread::spawn(move || {
                thread::yield_now();
                step.fetch_add(1, SeqCst)
            })
        };

        let other = {
            let step = step.clone();
            thread::spawn(move || step.fetch_add(10, SeqCst))
        };

        // Steps of this thread that have nothing to do with the others.
        let own = Arc::new(AtomicUsize::new(0));
        own.store(1, SeqCst);
        own.store(2, SeqCst);

        let before = step.fetch_add(100, SeqCst);

        (before, yielder.join().unwrap(), other.join().unwrap())
    });
}
