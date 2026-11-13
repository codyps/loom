#![deny(warnings, rust_2018_idioms)]
use loom::thread;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn thread_local() {
    loom::thread_local! {
        static THREAD_LOCAL: RefCell<usize> = RefCell::new(1);
    }

    fn do_test(n: usize) {
        THREAD_LOCAL.with(|local| {
            assert_eq!(*local.borrow(), 1);
        });
        THREAD_LOCAL.with(|local| {
            assert_eq!(*local.borrow(), 1);
            *local.borrow_mut() = n;
            assert_eq!(*local.borrow(), n);
        });
        THREAD_LOCAL.with(|local| {
            assert_eq!(*local.borrow(), n);
        });
    }

    loom::model(|| {
        let t1 = thread::spawn(|| do_test(2));

        let t2 = thread::spawn(|| do_test(3));

        do_test(4);

        t1.join().unwrap();
        t2.join().unwrap();
    });
}

#[test]
fn nested_with() {
    loom::thread_local! {
        static LOCAL1: RefCell<usize> = RefCell::new(1);
        static LOCAL2: RefCell<usize> = RefCell::new(2);
    }

    loom::model(|| {
        LOCAL1.with(|local1| *local1.borrow_mut() = LOCAL2.with(|local2| *local2.borrow()));
    });
}

#[test]
fn drop() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct CountDrops {
        drops: &'static AtomicUsize,
        dummy: bool,
    }

    impl Drop for CountDrops {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Release);
        }
    }

    impl CountDrops {
        fn new(drops: &'static AtomicUsize) -> Self {
            Self { drops, dummy: true }
        }
    }

    loom::thread_local! {
        static DROPPED_LOCAL: CountDrops = CountDrops::new(&DROPS);
    }

    loom::model(|| {
        assert_eq!(DROPS.load(Ordering::Acquire), 0);

        thread::spawn(|| {
            // force access to the thread local so that it's initialized.
            DROPPED_LOCAL.with(|local| assert!(local.dummy));
            assert_eq!(DROPS.load(Ordering::Acquire), 0);
        })
        .join()
        .unwrap();

        // When the first spawned thread completed, its copy of the thread local
        // should have been dropped.
        assert_eq!(DROPS.load(Ordering::Acquire), 1);

        thread::spawn(|| {
            // force access to the thread local so that it's initialized.
            DROPPED_LOCAL.with(|local| assert!(local.dummy));
            assert_eq!(DROPS.load(Ordering::Acquire), 1);
        })
        .join()
        .unwrap();

        // When the second spawned thread completed, its copy of the thread local
        // should have been dropped as well.
        assert_eq!(DROPS.load(Ordering::Acquire), 2);

        // force access to the thread local so that it's initialized.
        DROPPED_LOCAL.with(|local| assert!(local.dummy));
    });

    // Finally, when the model's "main thread" completes, its copy of the local
    // should also be dropped.
    assert_eq!(DROPS.load(Ordering::Acquire), 3);
}

/// Counts how often a lazy initializer ran, so tests can check that `set`
/// on an uninitialized key skips it (as `std` does).
fn bump(counter: &'static AtomicUsize) {
    counter.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn local_key_cell() {
    static INITS: AtomicUsize = AtomicUsize::new(0);

    loom::thread_local! {
        static LOCAL: Cell<usize> = { bump(&INITS); Cell::new(1) };
        static UNSET: Cell<usize> = { bump(&INITS); Cell::new(10) };
    }

    INITS.store(0, Ordering::Relaxed);

    loom::model(|| {
        let before = INITS.load(Ordering::Relaxed);

        // `set` on an uninitialized key must not run the initializer.
        LOCAL.set(2);
        assert_eq!(INITS.load(Ordering::Relaxed), before);
        assert_eq!(LOCAL.get(), 2);

        // ... and on an initialized key it overwrites.
        LOCAL.set(3);
        assert_eq!(LOCAL.replace(4), 3);
        assert_eq!(LOCAL.take(), 4);
        assert_eq!(LOCAL.get(), 0);

        // `get` on an uninitialized key runs the initializer.
        assert_eq!(UNSET.get(), 10);
        assert_eq!(INITS.load(Ordering::Relaxed), before + 1);

        // Each thread has its own value.
        thread::spawn(|| {
            assert_eq!(LOCAL.get(), 1);
            LOCAL.set(7);
            assert_eq!(LOCAL.get(), 7);
        })
        .join()
        .unwrap();

        assert_eq!(LOCAL.get(), 0);
    });
}

#[test]
fn local_key_loom_cell() {
    loom::thread_local! {
        static LOCAL: loom::cell::Cell<usize> = loom::cell::Cell::new(1);
        static UNSET: loom::cell::Cell<usize> = loom::cell::Cell::new(10);
    }

    loom::model(|| {
        // `set` before first use; this constructs a loom object and so must
        // not happen while the runtime is borrowed.
        LOCAL.set(2);
        assert_eq!(LOCAL.get(), 2);
        LOCAL.set(3);
        assert_eq!(LOCAL.replace(4), 3);
        assert_eq!(LOCAL.take(), 4);
        assert_eq!(LOCAL.get(), 0);
        assert_eq!(UNSET.get(), 10);

        thread::spawn(|| {
            assert_eq!(LOCAL.get(), 1);
        })
        .join()
        .unwrap();
    });
}

#[test]
fn local_key_ref_cell() {
    static INITS: AtomicUsize = AtomicUsize::new(0);

    loom::thread_local! {
        static LOCAL: RefCell<Vec<usize>> = { bump(&INITS); RefCell::new(vec![1]) };
    }

    INITS.store(0, Ordering::Relaxed);

    loom::model(|| {
        let before = INITS.load(Ordering::Relaxed);

        LOCAL.set(vec![2]);
        assert_eq!(INITS.load(Ordering::Relaxed), before);

        LOCAL.with_borrow(|v| assert_eq!(v, &[2]));
        LOCAL.with_borrow_mut(|v| v.push(3));
        assert_eq!(LOCAL.replace(vec![4]), vec![2, 3]);
        LOCAL.set(vec![5]);
        assert_eq!(LOCAL.take(), vec![5]);
        assert!(LOCAL.with_borrow(Vec::is_empty));

        thread::spawn(|| {
            LOCAL.with_borrow(|v| assert_eq!(v, &[1]));
        })
        .join()
        .unwrap();
    });
}
