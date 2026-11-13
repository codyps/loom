#![deny(warnings, rust_2018_idioms)]

use loom::cell::UnsafeCell;
use loom::sync::atomic::AtomicBool;
use loom::sync::atomic::Ordering::{Acquire, Release};
use loom::sync::Arc;
use loom::sync::Notify;
use loom::thread;

struct State {
    data: UnsafeCell<usize>,
    guard: AtomicBool,
}

impl Drop for State {
    fn drop(&mut self) {
        self.data.with(|ptr| unsafe {
            assert_eq!(1, *ptr);
        });
    }
}

#[test]
fn basic_usage() {
    loom::model(|| {
        let num = Arc::new(State {
            data: UnsafeCell::new(0),
            guard: AtomicBool::new(false),
        });

        let num2 = num.clone();
        thread::spawn(move || {
            num2.data.with_mut(|ptr| unsafe { *ptr = 1 });
            num2.guard.store(true, Release);
        });

        loop {
            if num.guard.load(Acquire) {
                num.data.with(|ptr| unsafe {
                    assert_eq!(1, *ptr);
                });
                break;
            }

            thread::yield_now();
        }
    });
}

#[test]
fn sync_in_drop() {
    loom::model(|| {
        let num = Arc::new(State {
            data: UnsafeCell::new(0),
            guard: AtomicBool::new(false),
        });

        let num2 = num.clone();
        thread::spawn(move || {
            num2.data.with_mut(|ptr| unsafe { *ptr = 1 });
            num2.guard.store(true, Release);
            drop(num2);
        });

        drop(num);
    });
}

#[test]
#[should_panic]
fn detect_mem_leak() {
    loom::model(|| {
        let num = Arc::new(State {
            data: UnsafeCell::new(0),
            guard: AtomicBool::new(false),
        });

        std::mem::forget(num);
    });
}

#[test]
fn try_unwrap_succeeds() {
    loom::model(|| {
        let num = Arc::new(0usize);
        let num2 = Arc::clone(&num);
        drop(num2);
        let _ = Arc::try_unwrap(num).unwrap();
    });
}

#[test]
fn try_unwrap_fails() {
    loom::model(|| {
        let num = Arc::new(0usize);
        let num2 = Arc::clone(&num);
        let num = Arc::try_unwrap(num).unwrap_err();

        drop(num2);

        let _ = Arc::try_unwrap(num).unwrap();
    });
}

#[test]
fn try_unwrap_multithreaded() {
    loom::model(|| {
        let num = Arc::new(0usize);
        let num2 = Arc::clone(&num);
        let can_drop = Arc::new(Notify::new());
        let thread = {
            let can_drop = can_drop.clone();
            thread::spawn(move || {
                can_drop.wait();
                drop(num2);
            })
        };

        // The other thread is holding the other arc clone, so we can't unwrap the arc.
        let num = Arc::try_unwrap(num).unwrap_err();

        // Allow the thread to proceed.
        can_drop.notify();

        // After the thread drops the other clone, the arc should be
        // unwrappable.
        thread.join().unwrap();
        let _ = Arc::try_unwrap(num).unwrap();
    });
}

#[test]
fn into_inner_unique() {
    loom::model(|| {
        let arc = Arc::new(String::from("hello"));
        assert_eq!(Arc::into_inner(arc).as_deref(), Some("hello"));
    });
}

/// When every handle is consumed with `into_inner`, exactly one caller gets
/// the value, no matter how the calls interleave (this is the guarantee that
/// distinguishes `into_inner` from `try_unwrap`).
#[test]
fn into_inner_exactly_one_winner() {
    loom::model(|| {
        let a = Arc::new(String::from("hello"));
        let b = a.clone();

        let th = thread::spawn(move || Arc::into_inner(b));

        let mine = Arc::into_inner(a);
        let theirs = th.join().unwrap();

        match (mine, theirs) {
            (Some(v), None) | (None, Some(v)) => assert_eq!(v, "hello"),
            other => panic!("expected exactly one winner, got {:?}", other),
        }
    });
}

/// `into_inner` on a non-unique handle releases that handle without dropping
/// the value; the remaining handle stays usable.
#[test]
fn into_inner_shared_keeps_value_alive() {
    loom::model(|| {
        let a = Arc::new(7usize);
        let b = a.clone();

        assert_eq!(Arc::into_inner(a), None);
        assert_eq!(Arc::strong_count(&b), 1);
        assert_eq!(*b, 7);
        assert_eq!(Arc::into_inner(b), Some(7));
    });
}

/// Test that there is no double panic
/// when a model with an Arc in an unspawned thread panics.
#[test]
#[should_panic(expected = "loom should not panic inside another panic")]
fn access_on_drop_during_panic_in_unspawned_thread() {
    use loom::sync::Arc;
    use std::panic::catch_unwind;

    let result = catch_unwind(|| {
        loom::model(move || {
            let arc = Arc::new(());
            thread::spawn(move || {
                let _arc = arc;
            });
            panic!();
        });
    });

    // propagate the panic from the spawned thread
    // to the main thread.
    result.expect("loom should not panic inside another panic");
}

/// `Arc<[T]>` can be collected into, as with `std`.
#[test]
fn slice_from_iterator() {
    loom::model(|| {
        let numbers: Arc<[usize]> = (1..=3).collect();
        assert_eq!(&*numbers, &[1, 2, 3]);

        let empty: Arc<[usize]> = std::iter::empty().collect();
        assert!(empty.is_empty());

        let from_vec: Arc<[String]> = Arc::from(vec![String::from("a")]);
        assert_eq!(from_vec.len(), 1);

        // The result is an ordinary, tracked `Arc`.
        let other = numbers.clone();
        let th = thread::spawn(move || other.iter().sum::<usize>());
        assert_eq!(th.join().unwrap(), 6);
        assert_eq!(Arc::strong_count(&numbers), 1);
    });
}
