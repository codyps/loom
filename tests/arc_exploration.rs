#![deny(warnings, rust_2018_idioms)]

//! Tests that loom explores every result an `Arc`'s reference count can be
//! observed to have, and every way channel operations can interleave.

use loom::sync::mpsc::channel;
use loom::sync::Arc;
use loom::thread;

use std::collections::BTreeSet;
use std::sync::mpsc::TryRecvError;
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

/// A count taken by one thread races with a clone by another, also when the
/// cloning thread looked at the count itself just before.
#[test]
fn strong_count_races_with_clone_after_inspection() {
    let seen = outcomes(|| {
        let a = Arc::new(());
        let b = a.clone();

        let t1 = thread::spawn(move || Arc::strong_count(&a));
        let t2 = thread::spawn(move || {
            let _ = Arc::strong_count(&b);
            let c = b.clone();
            // Keep both handles until the other thread is done counting.
            (b, c)
        });

        let count = t1.join().unwrap();
        drop(t2.join().unwrap());

        count
    });

    assert_eq!(seen, BTreeSet::from([2, 3]));
}

/// A count races with another thread dropping its handle.
#[test]
fn strong_count_races_with_drop() {
    let seen = outcomes(|| {
        let a = Arc::new(());
        let b = a.clone();

        let t1 = thread::spawn(move || Arc::strong_count(&a));
        let t2 = thread::spawn(move || drop(b));

        let count = t1.join().unwrap();
        t2.join().unwrap();

        count
    });

    assert_eq!(seen, BTreeSet::from([1, 2]));
}

/// A count races with a clone in one thread and a drop in another,
/// independently of each other.
#[test]
fn strong_count_races_with_clone_and_drop() {
    let seen = outcomes(|| {
        let a = Arc::new(());
        let b = a.clone();
        let c = a.clone();

        let t1 = thread::spawn(move || {
            let extra = b.clone();
            (b, extra)
        });
        let t2 = thread::spawn(move || drop(c));

        let count = Arc::strong_count(&a);

        drop(t1.join().unwrap());
        t2.join().unwrap();

        count
    });

    // 3 handles to start with; the clone adds one, the drop removes one.
    assert_eq!(seen, BTreeSet::from([2, 3, 4]));
}

/// Two threads counting while a third one clones: each count can come before
/// or after the clone, in any combination.
#[test]
fn two_strong_counts_race_with_clone() {
    let seen = outcomes(|| {
        let a = Arc::new(());
        let b = a.clone();
        let c = a.clone();

        // Both threads hand their handles back, so that only the clone
        // changes the count while the counting is going on.
        let t1 = thread::spawn(move || (Arc::strong_count(&b), b));
        let t2 = thread::spawn(move || {
            let extra = c.clone();
            (c, extra)
        });

        let mine = Arc::strong_count(&a);
        let (theirs, b) = t1.join().unwrap();
        drop(t2.join().unwrap());
        drop(b);

        (mine, theirs)
    });

    assert_eq!(seen, BTreeSet::from([(3, 3), (3, 4), (4, 3), (4, 4)]));
}

/// `try_unwrap` races with another handle being dropped.
#[test]
fn try_unwrap_races_with_drop() {
    let seen = outcomes(|| {
        let a = Arc::new(7);
        let b = a.clone();

        let th = thread::spawn(move || drop(b));

        let unwrapped = match Arc::try_unwrap(a) {
            Ok(_) => true,
            Err(a) => {
                drop(a);
                false
            }
        };

        th.join().unwrap();

        unwrapped
    });

    assert_eq!(seen, BTreeSet::from([false, true]));
}

/// Two senders racing: the receiver sees the messages in either order.
#[test]
fn senders_race() {
    let seen = outcomes(|| {
        let (s1, r) = channel();
        let s2 = s1.clone();

        let t1 = thread::spawn(move || s1.send(1).unwrap());
        let t2 = thread::spawn(move || s2.send(2).unwrap());

        let first = r.recv().unwrap();
        let second = r.recv().unwrap();

        t1.join().unwrap();
        t2.join().unwrap();

        (first, second)
    });

    assert_eq!(seen, BTreeSet::from([(1, 2), (2, 1)]));
}

/// Two `try_recv`s racing with two senders: every combination of "nothing
/// yet" and the messages, in either order.
#[test]
fn try_recvs_race_with_senders() {
    fn outcome(result: Result<i32, TryRecvError>) -> i32 {
        match result {
            Ok(message) => message,
            Err(TryRecvError::Empty) => 0,
            Err(TryRecvError::Disconnected) => -1,
        }
    }

    let seen = outcomes(|| {
        let (s1, r) = channel();
        let s2 = s1.clone();

        let t1 = thread::spawn(move || s1.send(1).unwrap());
        let t2 = thread::spawn(move || s2.send(2).unwrap());

        let first = outcome(r.try_recv());
        let second = outcome(r.try_recv());

        t1.join().unwrap();
        t2.join().unwrap();

        (first, second)
    });

    assert_eq!(
        seen,
        BTreeSet::from([
            (0, 0),
            (0, 1),
            (0, 2),
            (1, 0),
            (1, 2),
            (2, 0),
            (2, 1),
            // Both senders are gone after the second message at the earliest,
            // so a disconnect cannot be seen within two attempts ...
        ])
    );
}

/// ... but it can be with a third: after one sender has sent and both are
/// dropped.
#[test]
fn try_recv_sees_disconnect_after_messages() {
    let seen = outcomes(|| {
        let (s, r) = channel();

        let th = thread::spawn(move || {
            s.send(1).unwrap();
        });

        let mut results = Vec::new();
        for _ in 0..2 {
            results.push(match r.try_recv() {
                Ok(_) => 'm',
                Err(TryRecvError::Empty) => 'e',
                Err(TryRecvError::Disconnected) => 'd',
            });
        }

        th.join().unwrap();

        results.into_iter().collect::<String>()
    });

    assert_eq!(
        seen,
        ["ee", "em", "md", "me"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>()
    );
}

/// A sender finding the receiver gone races with a second sender doing the
/// same.
#[test]
fn sends_race_with_receiver_drop() {
    let seen = outcomes(|| {
        let (s1, r) = channel();
        let s2 = s1.clone();

        let t1 = thread::spawn(move || s1.send(1).is_ok());
        let t2 = thread::spawn(move || s2.send(2).is_ok());

        drop(r);

        (t1.join().unwrap(), t2.join().unwrap())
    });

    assert_eq!(
        seen,
        BTreeSet::from([(false, false), (false, true), (true, false), (true, true)])
    );
}

/// Whichever handle is dropped last frees the value, and runs its destructor
/// on that thread. Every handle gets to be the last one.
#[test]
fn every_handle_can_be_dropped_last() {
    struct DropOn(std::sync::Arc<Mutex<BTreeSet<String>>>);

    impl Drop for DropOn {
        fn drop(&mut self) {
            let thread = format!("{:?}", thread::current().id());
            self.0.lock().unwrap().insert(thread);
        }
    }

    let dropped_on = std::sync::Arc::new(Mutex::new(BTreeSet::new()));

    {
        let dropped_on = dropped_on.clone();
        loom::model(move || {
            let value = Arc::new(DropOn(dropped_on.clone()));

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
    }

    // The main thread and the three it spawned.
    assert_eq!(dropped_on.lock().unwrap().len(), 4, "{:?}", dropped_on);
}

/// Which handle goes last has nothing to do with what else the threads did:
/// the thread that frees the value can find any thread's store in `x`.
#[test]
fn last_drop_is_independent_of_other_operations() {
    use loom::sync::atomic::AtomicUsize;
    use loom::sync::atomic::Ordering::SeqCst;

    type Seen = std::sync::Arc<Mutex<BTreeSet<(String, usize)>>>;

    struct DropOn(Seen, std::sync::Arc<AtomicUsize>);

    impl Drop for DropOn {
        fn drop(&mut self) {
            let thread = format!("{:?}", thread::current().id());
            self.0.lock().unwrap().insert((thread, self.1.load(SeqCst)));
        }
    }

    let seen = Seen::default();

    {
        let seen = seen.clone();
        loom::model(move || {
            let x = std::sync::Arc::new(AtomicUsize::new(0));
            let value = Arc::new(DropOn(seen.clone(), x.clone()));

            let handles: Vec<_> = (1..=3)
                .map(|i| {
                    let x = x.clone();
                    let value = value.clone();
                    thread::spawn(move || {
                        x.store(i, SeqCst);
                        drop(value);
                    })
                })
                .collect();

            drop(value);

            for handle in handles {
                handle.join().unwrap();
            }
        });
    }

    let seen = seen.lock().unwrap();

    // A spawned thread frees the value after all three stores, the last of
    // which can be any of them: 3 x 3. The main thread does so after its own
    // stores only, that is none: it is last when the other handles are gone,
    // and by then the three stores have been made as well: 3 more.
    assert_eq!(seen.len(), 12, "{:?}", seen);
}
