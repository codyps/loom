#![cfg(feature = "lock_api")]
#![deny(warnings, rust_2018_idioms)]

use loom::cell::UnsafeCell;
use loom::lock_api::{Mutex, RwLock};
use loom::sync::atomic::AtomicUsize;
use loom::thread;

use std::rc::Rc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

#[test]
fn mutex_enforces_mutual_exclusion() {
    loom::model(|| {
        let data = Rc::new((Mutex::new(0), AtomicUsize::new(0)));

        let ths: Vec<_> = (0..2)
            .map(|_| {
                let data = data.clone();

                thread::spawn(move || {
                    let mut locked = data.0.lock();

                    let prev = data.1.fetch_add(1, SeqCst);
                    assert_eq!(prev, *locked);
                    *locked += 1;
                })
            })
            .collect();

        for th in ths {
            th.join().unwrap();
        }

        let locked = data.0.lock();

        assert_eq!(*locked, data.1.load(SeqCst));
    });
}

#[test]
fn mutex_establishes_seq_cst() {
    loom::model(|| {
        struct Data {
            cell: UnsafeCell<usize>,
            flag: Mutex<bool>,
        }

        let data = Rc::new(Data {
            cell: UnsafeCell::new(0),
            flag: Mutex::new(false),
        });

        {
            let data = data.clone();

            thread::spawn(move || {
                unsafe { data.cell.with_mut(|v| *v = 1) };
                *data.flag.lock() = true;
            });
        }

        let flag = *data.flag.lock();

        if flag {
            let v = unsafe { data.cell.with(|v| *v) };
            assert_eq!(v, 1);
        }
    });
}

#[test]
fn rwlock_read_one() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(1));
        let c_lock = lock.clone();

        let n = lock.read();
        assert_eq!(*n, 1);

        thread::spawn(move || {
            let _l = c_lock.read();
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
                let _l = lock.read();

                thread::yield_now();
            });
        }

        let _l = lock.write();
        thread::yield_now();
    });
}

#[test]
fn rwlock_try_read() {
    loom::model(|| {
        let lock = RwLock::new(1);

        match lock.try_read() {
            Some(n) => assert_eq!(*n, 1),
            None => unreachable!(),
        };
    });
}

#[test]
fn rwlock_write() {
    loom::model(|| {
        let lock = RwLock::new(1);

        let mut n = lock.write();
        *n = 2;

        assert!(lock.try_read().is_none());
    });
}

#[test]
fn rwlock_try_write() {
    loom::model(|| {
        let lock = RwLock::new(1);

        let n = lock.read();
        assert_eq!(*n, 1);

        assert!(lock.try_write().is_none());
    });
}

#[test]
fn mutex_try_lock_and_is_locked() {
    loom::model(|| {
        let lock = Mutex::new(1);
        assert!(!lock.is_locked());

        let guard = lock.try_lock().unwrap();
        assert!(lock.is_locked());
        assert!(lock.try_lock().is_none());

        drop(guard);
        assert!(!lock.is_locked());
    });
}

/// `try_lock` racing with a holder is explored both ways, and never reported
/// as a deadlock.
#[test]
fn mutex_try_lock_explores_both_outcomes() {
    use std::sync::atomic::AtomicBool;

    let saw_some = Arc::new(AtomicBool::new(false));
    let saw_none = Arc::new(AtomicBool::new(false));

    {
        let saw_some = saw_some.clone();
        let saw_none = saw_none.clone();
        loom::model(move || {
            let lock = Rc::new(Mutex::new(0));
            let lock2 = lock.clone();

            let th = thread::spawn(move || {
                let guard = lock2.lock();
                thread::yield_now();
                drop(guard);
            });

            match lock.try_lock() {
                Some(_) => saw_some.store(true, SeqCst),
                None => saw_none.store(true, SeqCst),
            }

            th.join().unwrap();
        });
    }

    assert!(saw_some.load(SeqCst));
    assert!(saw_none.load(SeqCst));
}

/// The parts of the API that `std`'s locks do not have.
#[test]
fn mapped_guards() {
    loom::model(|| {
        let lock = Mutex::new((1, 2));
        {
            let mut second = loom::lock_api::MutexGuard::map(lock.lock(), |pair| &mut pair.1);
            *second += 1;
        }
        assert_eq!(*lock.lock(), (1, 3));

        let rwlock = RwLock::new((1, 2));
        {
            let first = loom::lock_api::RwLockReadGuard::map(rwlock.read(), |pair| &pair.0);
            assert_eq!(*first, 1);
            assert!(rwlock.try_write().is_none());
        }
        *rwlock.write() = (5, 6);
        assert_eq!(*rwlock.read(), (5, 6));
    });
}

/// As for the `std` style mutex: an attempt can land inside a hold during
/// which the holder does nothing else.
#[test]
fn mutex_try_lock_inside_empty_critical_section() {
    use std::sync::atomic::AtomicBool;

    let saw_some = Arc::new(AtomicBool::new(false));
    let saw_none = Arc::new(AtomicBool::new(false));

    {
        let saw_some = saw_some.clone();
        let saw_none = saw_none.clone();
        loom::model(move || {
            let lock = Rc::new(Mutex::new(0));
            let lock2 = lock.clone();

            let th = thread::spawn(move || {
                drop(lock2.lock());
            });

            match lock.try_lock() {
                Some(_) => saw_some.store(true, SeqCst),
                None => saw_none.store(true, SeqCst),
            }

            th.join().unwrap();
        });
    }

    assert!(saw_some.load(SeqCst));
    assert!(saw_none.load(SeqCst));
}

/// `parking_lot` style downgrade of a write guard.
#[test]
fn rwlock_downgrade() {
    use loom::lock_api::RwLockWriteGuard;

    loom::model(|| {
        let lock = Arc::new(RwLock::new(0));

        let mut write = lock.write();
        *write = 1;
        let read = RwLockWriteGuard::downgrade(write);

        assert_eq!(*lock.try_read().unwrap(), 1);
        assert!(lock.try_write().is_none());

        let th = {
            let lock = lock.clone();
            thread::spawn(move || *lock.read())
        };
        assert_eq!(th.join().unwrap(), 1);

        drop(read);
        assert!(lock.try_write().is_some());
    });
}
