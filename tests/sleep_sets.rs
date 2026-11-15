#![deny(warnings, rust_2018_idioms)]

//! Skipping executions that repeat explored ones must not change what loom
//! finds. Each model here is run with and without it, and has to produce the
//! same outcomes, in no more executions. So does following wakeup trees,
//! which goes one step further. That goes for a search with a
//! preemption bound as well, which finds less to begin with: it must not
//! find less still.

use loom::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use loom::sync::atomic::{AtomicBool, AtomicUsize};
use loom::sync::mpsc::channel;
use loom::sync::{Arc, Condvar, Mutex, Notify, RwLock};
use loom::thread;

use std::collections::BTreeSet;
use std::sync::atomic::AtomicUsize as StdAtomicUsize;

/// How much of what loom does to explore less is turned on.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Plain,
    SleepSets,
    WakeupTrees,
}

/// Runs `f` under loom and returns the outcomes and the number of executions.
fn explore<T, F>(mode: Mode, bound: Option<usize>, f: F) -> (BTreeSet<T>, usize)
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
        builder.sleep_sets = mode != Mode::Plain;
        builder.wakeup_trees = mode == Mode::WakeupTrees;

        // Wakeup trees are not used with several workers (`LOOM_WORKERS`).
        if mode == Mode::WakeupTrees {
            builder.workers = 1;
        }

        builder.check(move || {
            count.fetch_add(1, SeqCst);
            let outcome = f();
            seen.lock().unwrap().insert(outcome);
        });
    }

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    (seen, count.load(SeqCst))
}

/// Checks that `f` has the same outcomes either way, and returns them.
fn same_outcomes<T, F>(f: F) -> BTreeSet<T>
where
    T: Ord + Send + std::fmt::Debug + 'static,
    F: Fn() -> T + Sync + Send + Copy + 'static,
{
    // With a bound, the number of executions is not compared: a thread that
    // is asleep where the search wants to try it makes the search try the
    // other threads there, which now and then costs more than the sleeping
    // saves.
    for bound in [Some(0), Some(1), Some(2), Some(3)] {
        let (with, _) = explore(Mode::SleepSets, bound, f);
        let (without, _) = explore(Mode::Plain, bound, f);

        assert_eq!(with, without, "preemption bound {:?}", bound);
    }

    let (with, with_count) = explore(Mode::SleepSets, None, f);
    let (without, without_count) = explore(Mode::Plain, None, f);

    assert_eq!(with, without);
    assert!(
        with_count <= without_count,
        "{} executions with, {} without",
        with_count,
        without_count
    );

    // Wakeup trees are not compared by the number of executions with sleep
    // sets alone: nearly always fewer, now and then one or two more.
    let (trees, trees_count) = explore(Mode::WakeupTrees, None, f);

    assert_eq!(trees, without, "wakeup trees");
    assert!(
        trees_count <= without_count,
        "{} executions with wakeup trees, {} without",
        trees_count,
        without_count
    );

    with
}

#[test]
fn racing_stores_and_loads() {
    let seen = same_outcomes(|| {
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (1..=2)
            .map(|id| {
                let x = x.clone();
                thread::spawn(move || {
                    let seen = x.load(Relaxed);
                    x.store(seen * 10 + id, Relaxed);
                    seen
                })
            })
            .collect();

        let mine = x.load(Relaxed);
        let theirs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        (mine, theirs, x.load(Relaxed))
    });

    assert!(seen.len() > 4);
}

#[test]
fn mutex_and_try_lock() {
    let seen = same_outcomes(|| {
        let m = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (1..=2)
            .map(|id| {
                let m = m.clone();
                thread::spawn(move || {
                    m.lock().unwrap().push(id);
                    m.lock().unwrap().push(id + 10);
                })
            })
            .collect();

        let got_it = match m.try_lock() {
            Ok(mut guard) => {
                guard.push(0);
                true
            }
            Err(_) => false,
        };

        for handle in handles {
            handle.join().unwrap();
        }

        let order = m.lock().unwrap().clone();
        (got_it, order)
    });

    // 2 threads with two critical sections each: 6 orders, times where the
    // main thread's attempt lands.
    assert!(seen.iter().any(|(got_it, _)| *got_it));
    assert!(seen.iter().any(|(got_it, _)| !*got_it));
    assert!(seen.len() >= 6);
}

#[test]
fn rwlock_readers_and_writer() {
    same_outcomes(|| {
        let lock = Arc::new(RwLock::new(0usize));

        let readers: Vec<_> = (0..2)
            .map(|_| {
                let lock = lock.clone();
                thread::spawn(move || *lock.read().unwrap())
            })
            .collect();

        *lock.write().unwrap() += 1;
        let tried = lock.try_read().map(|guard| *guard).ok();

        let read: Vec<_> = readers.into_iter().map(|h| h.join().unwrap()).collect();
        (read, tried)
    });
}

#[test]
fn condvar_handoff() {
    same_outcomes(|| {
        let state = Arc::new((Mutex::new(0usize), Condvar::new()));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let state = state.clone();
                thread::spawn(move || {
                    let (mutex, condvar) = &*state;
                    let mut guard = mutex.lock().unwrap();
                    *guard += 1;
                    let seen = *guard;
                    condvar.notify_all();
                    seen
                })
            })
            .collect();

        let (mutex, condvar) = &*state;
        let mut guard = mutex.lock().unwrap();
        let mut waits = 0;
        while *guard < 2 {
            guard = condvar.wait(guard).unwrap();
            waits += 1;
        }
        drop(guard);

        let order: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        (order, waits)
    });
}

#[test]
fn channel_with_two_senders() {
    let seen = same_outcomes(|| {
        let (tx, rx) = channel();
        let tx2 = tx.clone();

        let t1 = thread::spawn(move || tx.send(1).unwrap());
        let t2 = thread::spawn(move || tx2.send(2).unwrap());

        let early = rx.try_recv().ok();
        let mut rest = Vec::new();
        while let Ok(message) = rx.recv() {
            rest.push(message);
        }

        t1.join().unwrap();
        t2.join().unwrap();

        (early, rest)
    });

    assert!(seen.len() >= 4);
}

#[test]
fn flag_published_with_release() {
    same_outcomes(|| {
        let data = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));

        let writer = {
            let (data, ready) = (data.clone(), ready.clone());
            thread::spawn(move || {
                data.store(42, Relaxed);
                ready.store(true, Release);
            })
        };

        let reader = {
            let (data, ready) = (data.clone(), ready.clone());
            thread::spawn(move || {
                if ready.load(Acquire) {
                    Some(data.load(Relaxed))
                } else {
                    None
                }
            })
        };

        let mine = ready.load(Relaxed);
        writer.join().unwrap();

        (mine, reader.join().unwrap())
    });
}

#[test]
fn store_buffering_seq_cst() {
    let seen = same_outcomes(|| {
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
                y.store(1, SeqCst);
                x.load(SeqCst)
            })
        };

        (t1.join().unwrap(), t2.join().unwrap())
    });

    assert_eq!(seen, BTreeSet::from([(0, 1), (1, 0), (1, 1)]));
}

#[test]
fn arc_counts_and_last_drop() {
    same_outcomes(|| {
        let value = Arc::new(());

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let value = value.clone();
                thread::spawn(move || {
                    let count = Arc::strong_count(&value);
                    drop(value);
                    count
                })
            })
            .collect();

        let mine = Arc::strong_count(&value);
        let theirs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        (mine, theirs)
    });
}

#[test]
fn notify_and_park() {
    same_outcomes(|| {
        let notify = Arc::new(Notify::new());
        let turns = Arc::new(AtomicUsize::new(0));
        let main = thread::current();

        let waiter = {
            let (notify, turns) = (notify.clone(), turns.clone());
            thread::spawn(move || {
                notify.wait();
                let seen = turns.fetch_add(1, SeqCst);
                main.unpark();
                seen
            })
        };

        let before = turns.fetch_add(1, SeqCst);
        notify.notify_one();

        while turns.load(SeqCst) < 2 {
            thread::park();
        }

        (before, waiter.join().unwrap())
    });
}

/// A spin loop with `yield_now`, which gets special treatment by the
/// scheduler.
#[test]
fn spin_until_flag() {
    same_outcomes(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let value = Arc::new(AtomicUsize::new(0));

        let setter = {
            let (flag, value) = (flag.clone(), value.clone());
            thread::spawn(move || {
                value.store(1, Relaxed);
                flag.store(true, Release);
                value.store(2, Relaxed);
            })
        };

        let mut spins = 0;
        while !flag.load(Acquire) {
            spins += 1;
            thread::yield_now();
        }

        let seen = value.load(Relaxed);
        setter.join().unwrap();

        (spins.min(2), seen)
    });
}

/// A chain of dependencies through three threads: for the first thread to
/// read 3 and the second to read 1, the third thread's store has to come
/// first, then all of the first thread, then the second.
///
/// The search gets there by wanting the first thread's store to `y` before
/// the second thread's load of it, at a point where the first thread is
/// asleep. What has to run from there is the third thread, which neither
/// the load nor the store has anything to do with.
#[test]
fn reordering_needs_another_thread_first() {
    let seen = same_outcomes(|| {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let t1 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                let read = x.load(SeqCst);
                y.store(1, SeqCst);
                read
            })
        };
        let t2 = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                let read = y.load(SeqCst);
                x.store(2, SeqCst);
                read
            })
        };
        let t3 = {
            let x = x.clone();
            thread::spawn(move || x.store(3, Relaxed))
        };

        let read = (t1.join().unwrap(), t2.join().unwrap());
        t3.join().unwrap();

        (read, x.load(Relaxed))
    });

    assert!(seen.contains(&((3, 1), 2)), "{:?}", seen);
    assert_eq!(seen.len(), 9, "{:?}", seen);
}

/// Senders that go away on their own threads, the last of which disconnects
/// the channel, and a receiver that looks without waiting.
#[test]
fn channel_disconnected_by_sender_threads() {
    use std::sync::mpsc::TryRecvError;

    let seen = same_outcomes(|| {
        let (tx, rx) = channel();

        let handles: Vec<_> = (0..2)
            .map(|i| {
                let tx = tx.clone();
                thread::spawn(move || tx.send(i).unwrap())
            })
            .collect();

        drop(tx);

        let received: Vec<_> = (0..3)
            .map(|_| match rx.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => 8,
                Err(TryRecvError::Disconnected) => 9,
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        received
    });

    // Both messages and then the disconnect, in either order of the
    // messages; or anything short of that, with the channel found empty
    // at any point.
    assert!(seen.contains(&vec![0, 1, 9]) && seen.contains(&vec![1, 0, 9]));
    assert!(seen.contains(&vec![8, 8, 8]));
    assert_eq!(seen.len(), 15, "{:?}", seen);
}

/// Messages arrive in every order the senders can send them in, whatever
/// else the senders do after dropping their handle.
#[test]
fn channel_drained_until_disconnected() {
    let seen = same_outcomes(|| {
        let (tx, rx) = channel();
        let x = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (1..=2)
            .map(|i| {
                let tx = tx.clone();
                let x = x.clone();

                thread::spawn(move || {
                    tx.send(i).unwrap();
                    drop(tx);
                    x.store(i, SeqCst);
                })
            })
            .collect();

        drop(tx);

        let mut received = vec![];

        while let Ok(message) = rx.recv() {
            received.push(message);
        }

        received.push(x.load(SeqCst));

        for handle in handles {
            handle.join().unwrap();
        }

        received
    });

    // Two orders of the messages, and three values of `x` with each.
    assert_eq!(seen.len(), 6, "{:?}", seen);
}

/// Readers that take the lock twice each see the writer's two increments at
/// any point.
#[test]
fn rwlock_readers_between_writes() {
    let seen = same_outcomes(|| {
        let lock = Arc::new(RwLock::new(0usize));

        let readers: Vec<_> = (0..2)
            .map(|_| {
                let lock = lock.clone();
                thread::spawn(move || {
                    let first = *lock.read().unwrap();
                    let second = *lock.read().unwrap();
                    (first, second)
                })
            })
            .collect();

        let writer = {
            let lock = lock.clone();
            thread::spawn(move || {
                *lock.write().unwrap() += 1;
                *lock.write().unwrap() += 1;
            })
        };

        let read: Vec<_> = readers
            .into_iter()
            .map(|reader| reader.join().unwrap())
            .collect();
        writer.join().unwrap();

        read
    });

    // Each reader sees one of the six non-decreasing pairs of 0, 1 and 2.
    assert_eq!(seen.len(), 36, "{:?}", seen);
}

/// The first thread reads `x` after the third has stored to it, and the
/// second reads `y` after the first has stored to that. To get there the
/// search has to move the first thread's load ahead of the second thread's
/// fence, which it had already done in another part of the search, where
/// the first thread ran before the others. It has to do it here as well:
/// what comes next is the second thread's load after the first thread's
/// store, and that is only seen from here.
#[test]
fn reordering_that_another_part_of_the_search_covers() {
    use loom::sync::atomic::fence;

    let seen = same_outcomes(|| {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let first = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                let read = x.load(SeqCst);
                y.store(1, Relaxed);
                read
            })
        };
        let second = {
            let y = y.clone();
            thread::spawn(move || {
                let read = y.load(Relaxed);
                fence(SeqCst);
                read
            })
        };
        let third = {
            let x = x.clone();
            thread::spawn(move || x.store(2, Relaxed))
        };

        let read = (first.join().unwrap(), second.join().unwrap());
        third.join().unwrap();

        read
    });

    assert_eq!(seen, BTreeSet::from([(0, 0), (0, 1), (2, 0), (2, 1)]));
}
