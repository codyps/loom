#![deny(warnings, rust_2018_idioms)]

use loom::cell::UnsafeCell;
use loom::sync::atomic::AtomicUsize;
use loom::sync::Mutex;
use loom::thread;

use std::rc::Rc;
use std::sync::atomic::Ordering::SeqCst;

#[test]
fn mutex_enforces_mutual_exclusion() {
    loom::model(|| {
        let data = Rc::new((Mutex::new(0), AtomicUsize::new(0)));

        let ths: Vec<_> = (0..2)
            .map(|_| {
                let data = data.clone();

                thread::spawn(move || {
                    let mut locked = data.0.lock().unwrap();

                    let prev = data.1.fetch_add(1, SeqCst);
                    assert_eq!(prev, *locked);
                    *locked += 1;
                })
            })
            .collect();

        for th in ths {
            th.join().unwrap();
        }

        let locked = data.0.lock().unwrap();

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
                *data.flag.lock().unwrap() = true;
            });
        }

        let flag = *data.flag.lock().unwrap();

        if flag {
            let v = unsafe { data.cell.with(|v| *v) };
            assert_eq!(v, 1);
        }
    });
}

#[test]
fn mutex_into_inner() {
    loom::model(|| {
        let lock = Rc::new(Mutex::new(0));

        let ths: Vec<_> = (0..2)
            .map(|_| {
                let lock = lock.clone();

                thread::spawn(move || {
                    *lock.lock().unwrap() += 1;
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

/// A thread that is about to `try_lock` never waits for the lock, so it must
/// not be treated as blocked when another thread acquires it first.
#[test]
fn try_lock_no_block() {
    loom::model(|| {
        let lock = Rc::new(Mutex::new(0));
        let lock2 = lock.clone();

        let th = thread::spawn(move || {
            let _ = lock.try_lock();
        });

        let guard = lock2.try_lock();

        th.join().unwrap();

        drop(guard)
    })
}

/// Same as above, with the lock taken by a blocking `lock`.
#[test]
fn try_lock_no_block_against_lock() {
    loom::model(|| {
        let lock = Rc::new(Mutex::new(0));
        let lock2 = lock.clone();

        let th = thread::spawn(move || lock.try_lock().is_ok());

        let guard = lock2.lock().unwrap();

        // The spawned thread can only have succeeded if it ran (and released
        // the lock) before we acquired it; either way it must finish while we
        // still hold the guard.
        let _ = th.join().unwrap();

        drop(guard)
    })
}

/// `try_lock` racing with a lock holder must be explored both ways.
#[test]
fn try_lock_explores_both_outcomes() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::Arc;

    let saw_ok = Arc::new(AtomicBool::new(false));
    let saw_err = Arc::new(AtomicBool::new(false));

    {
        let saw_ok = saw_ok.clone();
        let saw_err = saw_err.clone();
        loom::model(move || {
            let lock = Rc::new(Mutex::new(0));
            let lock2 = lock.clone();

            let th = thread::spawn(move || {
                let guard = lock2.lock().unwrap();
                thread::yield_now();
                drop(guard);
            });

            match lock.try_lock() {
                Ok(_) => saw_ok.store(true, SeqCst),
                Err(_) => saw_err.store(true, SeqCst),
            }

            th.join().unwrap();
        });
    }

    assert!(saw_ok.load(SeqCst), "try_lock never succeeded");
    assert!(saw_err.load(SeqCst), "try_lock never failed");
}
