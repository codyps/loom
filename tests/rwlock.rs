use loom::sync::{Arc, RwLock, TryLockResult};
use loom::thread;

use std::rc::Rc;
use std::sync::TryLockError;

#[test]
fn rwlock_read_one() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(1));
        let c_lock = lock.clone();

        let n = lock.read().unwrap();
        assert_eq!(*n, 1);

        thread::spawn(move || {
            let r = c_lock.read();
            assert!(r.is_ok());
        })
        .join()
        .unwrap();
    });
}

#[test]
fn rwlock_read_two_write_one() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(1));

        for _ in 0..2 {
            let lock = lock.clone();

            thread::spawn(move || {
                let _l = lock.read().unwrap();

                thread::yield_now();
            });
        }

        let _l = lock.write().unwrap();
        thread::yield_now();
    });
}

#[test]
fn rwlock_write_three() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(1));

        for _ in 0..2 {
            let lock = lock.clone();
            thread::spawn(move || {
                let _l = lock.write().unwrap();

                thread::yield_now();
            });
        }

        let _l = lock.write().unwrap();
        thread::yield_now();
    });
}

#[test]
fn rwlock_write_then_try_write() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(1));

        let _l1 = lock.write().unwrap();

        assert!(matches!(
            lock.try_write(),
            TryLockResult::Err(TryLockError::WouldBlock)
        ));
    });
}

#[test]
fn rwlock_write_then_try_read() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(1));

        let _l1 = lock.write().unwrap();

        assert!(matches!(
            lock.try_read(),
            TryLockResult::Err(TryLockError::WouldBlock)
        ));
    });
}

#[test]
fn rwlock_read_then_try_write() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(1));

        let _l1 = lock.write().unwrap();

        assert!(matches!(
            lock.try_write(),
            TryLockResult::Err(TryLockError::WouldBlock)
        ));
    });
}

#[test]
fn rwlock_try_read() {
    loom::model(|| {
        let lock = RwLock::new(1);

        match lock.try_read() {
            Ok(n) => assert_eq!(*n, 1),
            Err(_) => unreachable!(),
        };
    });
}

#[test]
fn rwlock_write() {
    loom::model(|| {
        let lock = RwLock::new(1);

        let mut n = lock.write().unwrap();
        *n = 2;

        assert!(lock.try_read().is_err());
    });
}

#[test]
fn rwlock_try_write() {
    loom::model(|| {
        let lock = RwLock::new(1);

        let n = lock.read().unwrap();
        assert_eq!(*n, 1);

        assert!(lock.try_write().is_err());
    });
}

#[test]
fn rwlock_into_inner() {
    loom::model(|| {
        let lock = Rc::new(RwLock::new(0));

        let ths: Vec<_> = (0..2)
            .map(|_| {
                let lock = lock.clone();

                thread::spawn(move || {
                    *lock.write().unwrap() += 1;
                })
            })
            .collect();

        for th in ths {
            th.join().unwrap();
        }

        let lock = Rc::try_unwrap(lock).unwrap().into_inner().unwrap();
        assert_eq!(lock, 2);
    })
}

/// A thread may hold several read guards at once; the lock stays read-locked
/// until the last of them is dropped.
#[test]
fn rwlock_read_twice_same_thread() {
    loom::model(|| {
        let lock = RwLock::new(1);

        let r1 = lock.read().unwrap();
        let r2 = lock.read().unwrap();
        assert_eq!(*r1, 1);
        assert_eq!(*r2, 1);

        drop(r1);
        assert!(lock.try_write().is_err());

        drop(r2);
        assert!(lock.try_write().is_ok());
    });
}

#[test]
fn rwlock_read_twice_same_thread_blocks_writer() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(1));

        let r1 = lock.read().unwrap();
        let r2 = lock.read().unwrap();

        let c_lock = lock.clone();
        let th = thread::spawn(move || {
            *c_lock.write().unwrap() = 2;
        });

        drop(r1);
        thread::yield_now();
        assert_eq!(*r2, 1);

        drop(r2);
        th.join().unwrap();
        assert_eq!(*lock.read().unwrap(), 2);
    });
}

/// A thread that is about to `try_read`/`try_write` never waits for the lock,
/// so it must not be treated as blocked when another thread acquires it first.
#[test]
fn rwlock_try_write_no_block() {
    loom::model(|| {
        let lock = Rc::new(RwLock::new(0));
        let lock2 = lock.clone();

        let th = thread::spawn(move || {
            let _ = lock.try_read();
        });

        let guard = lock2.try_write();

        th.join().unwrap();

        drop(guard)
    })
}

#[test]
fn rwlock_try_write_no_block_against_read() {
    loom::model(|| {
        let lock = Rc::new(RwLock::new(0));
        let lock2 = lock.clone();

        let th = thread::spawn(move || {
            let _ = lock.try_write();
        });

        let guard = lock2.read().unwrap();

        th.join().unwrap();

        drop(guard)
    })
}

#[test]
fn rwlock_try_write_no_block_against_write() {
    loom::model(|| {
        let lock = Rc::new(RwLock::new(0));
        let lock2 = lock.clone();

        let th = thread::spawn(move || {
            let _ = lock.try_write();
        });

        let guard = lock2.write().unwrap();

        th.join().unwrap();

        drop(guard)
    })
}

/// Outcomes of an attempt racing with a holder that does nothing between
/// acquiring and releasing: the attempt can land inside the hold.
fn try_inside_empty_hold(
    hold: fn(&RwLock<usize>),
    attempt: fn(&RwLock<usize>) -> bool,
) -> (bool, bool) {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

    let saw = std::sync::Arc::new((AtomicBool::new(false), AtomicBool::new(false)));

    {
        let saw = saw.clone();
        loom::model(move || {
            let lock = Rc::new(RwLock::new(0));
            let lock2 = lock.clone();

            let th = thread::spawn(move || hold(&lock2));

            if attempt(&lock) {
                saw.0.store(true, SeqCst);
            } else {
                saw.1.store(true, SeqCst);
            }

            th.join().unwrap();
        });
    }

    (saw.0.load(SeqCst), saw.1.load(SeqCst))
}

#[test]
fn rwlock_try_read_inside_empty_write_hold() {
    let (succeeded, failed) = try_inside_empty_hold(
        |lock| drop(lock.write().unwrap()),
        |lock| lock.try_read().is_ok(),
    );

    assert!(succeeded, "try_read never succeeded");
    assert!(failed, "try_read never failed");
}

#[test]
fn rwlock_try_write_inside_empty_read_hold() {
    let (succeeded, failed) = try_inside_empty_hold(
        |lock| drop(lock.read().unwrap()),
        |lock| lock.try_write().is_ok(),
    );

    assert!(succeeded, "try_write never succeeded");
    assert!(failed, "try_write never failed");
}

#[test]
fn rwlock_try_write_inside_empty_write_hold() {
    let (succeeded, failed) = try_inside_empty_hold(
        |lock| drop(lock.write().unwrap()),
        |lock| lock.try_write().is_ok(),
    );

    assert!(succeeded, "try_write never succeeded");
    assert!(failed, "try_write never failed");
}

/// Downgrading keeps the lock held: readers get in, writers do not, and what
/// was written before the downgrade is what every reader sees.
#[test]
fn rwlock_downgrade() {
    use loom::sync::RwLockWriteGuard;

    loom::model(|| {
        let lock = Arc::new(RwLock::new(0));

        let mut write = lock.write().unwrap();
        *write = 1;

        let read = RwLockWriteGuard::downgrade(write);
        assert_eq!(*read, 1);

        // Shared with other readers ...
        assert_eq!(*lock.try_read().unwrap(), 1);
        // ... but still locked against writers.
        assert!(lock.try_write().is_err());

        drop(read);
        assert!(lock.try_write().is_ok());
    });
}

/// A writer on another thread can never get in between the write and the
/// downgraded read: the value written is still there under the read guard.
#[test]
fn rwlock_downgrade_is_atomic() {
    use loom::sync::RwLockWriteGuard;

    loom::model(|| {
        let lock = Arc::new(RwLock::new(0));

        let th = {
            let lock = lock.clone();
            thread::spawn(move || {
                *lock.write().unwrap() = 100;
            })
        };

        let mut write = lock.write().unwrap();
        *write = 1;
        let read = RwLockWriteGuard::downgrade(write);

        // Had the lock been released in between, the other writer could
        // have replaced the value.
        assert_eq!(*read, 1);
        thread::yield_now();
        assert_eq!(*read, 1);
        drop(read);

        th.join().unwrap();
    });
}

/// A reader blocked behind the writer is woken by the downgrade.
#[test]
fn rwlock_downgrade_wakes_readers() {
    use loom::sync::RwLockWriteGuard;

    loom::model(|| {
        let lock = Arc::new(RwLock::new(0));

        let write = lock.write().unwrap();

        let th = {
            let lock = lock.clone();
            thread::spawn(move || *lock.read().unwrap())
        };

        let read = RwLockWriteGuard::downgrade(write);

        // The reader has to be able to finish while the read guard is held.
        assert_eq!(th.join().unwrap(), 0);
        drop(read);
    });
}
