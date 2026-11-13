#![deny(warnings, rust_2018_idioms)]

use loom::cell::UnsafeCell;
use loom::sync::atomic::AtomicUsize;
use loom::thread;

use std::sync::atomic::Ordering::{Acquire, Release};
use std::sync::Arc;

#[test]
fn atomic_causality_success() {
    struct Chan {
        data: UnsafeCell<usize>,
        guard: AtomicUsize,
    }

    impl Chan {
        fn set(&self) {
            unsafe {
                self.data.with_mut(|v| {
                    *v += 123;
                });
            }

            self.guard.store(1, Release);
        }

        fn get(&self) {
            if 0 == self.guard.load(Acquire) {
                return;
            }

            unsafe {
                self.data.with(|v| {
                    assert_eq!(*v, 123);
                });
            }
        }
    }

    loom::model(|| {
        let chan = Arc::new(Chan {
            data: UnsafeCell::new(0),
            guard: AtomicUsize::new(0),
        });

        let th = {
            let chan = chan.clone();
            thread::spawn(move || {
                chan.set();
            })
        };

        // Try getting before joining
        chan.get();

        th.join().unwrap();

        chan.get();
    });
}

#[test]
#[should_panic]
fn atomic_causality_fail() {
    struct Chan {
        data: UnsafeCell<usize>,
        guard: AtomicUsize,
    }

    impl Chan {
        fn set(&self) {
            unsafe {
                self.data.with_mut(|v| {
                    *v += 123;
                });
            }

            self.guard.store(1, Release);
        }

        fn get(&self) {
            unsafe {
                self.data.with(|v| {
                    assert_eq!(*v, 123);
                });
            }
        }
    }

    loom::model(|| {
        let chan = Arc::new(Chan {
            data: UnsafeCell::new(0),
            guard: AtomicUsize::new(0),
        });

        let th = {
            let chan = chan.clone();
            thread::spawn(move || chan.set())
        };

        // Try getting before joining
        chan.get();

        th.join().unwrap();

        chan.get();
    });
}

#[derive(Clone)]
struct Data(Arc<UnsafeCell<usize>>);

impl Data {
    fn new(v: usize) -> Self {
        Data(Arc::new(UnsafeCell::new(v)))
    }

    fn get(&self) -> usize {
        self.0.with(|v| unsafe { *v })
    }

    fn inc(&self) -> usize {
        self.0.with_mut(|v| unsafe {
            *v += 1;
            *v
        })
    }
}

#[test]
#[should_panic]
fn unsafe_cell_race_mut_mut_1() {
    loom::model(|| {
        let x = Data::new(1);
        let y = x.clone();

        let th1 = thread::spawn(move || x.inc());
        y.inc();

        th1.join().unwrap();

        assert_eq!(4, y.inc());
    });
}

#[test]
#[should_panic]
fn unsafe_cell_race_mut_mut_2() {
    loom::model(|| {
        let x = Data::new(1);
        let y = x.clone();
        let z = x.clone();

        let th1 = thread::spawn(move || x.inc());
        let th2 = thread::spawn(move || y.inc());

        th1.join().unwrap();
        th2.join().unwrap();

        assert_eq!(4, z.inc());
    });
}

#[test]
#[should_panic]
fn unsafe_cell_race_mut_immut_1() {
    loom::model(|| {
        let x = Data::new(1);
        let y = x.clone();

        let th1 = thread::spawn(move || assert_eq!(2, x.inc()));
        y.get();

        th1.join().unwrap();

        assert_eq!(3, y.inc());
    });
}

#[test]
#[should_panic]
fn unsafe_cell_race_mut_immut_2() {
    loom::model(|| {
        let x = Data::new(1);
        let y = x.clone();

        let th1 = thread::spawn(move || x.get());
        assert_eq!(2, y.inc());

        th1.join().unwrap();

        assert_eq!(3, y.inc());
    });
}

#[test]
#[should_panic]
fn unsafe_cell_race_mut_immut_3() {
    loom::model(|| {
        let x = Data::new(1);
        let y = x.clone();
        let z = x.clone();

        let th1 = thread::spawn(move || assert_eq!(2, x.inc()));
        let th2 = thread::spawn(move || y.get());

        th1.join().unwrap();
        th2.join().unwrap();

        assert_eq!(3, z.inc());
    });
}

#[test]
#[should_panic]
fn unsafe_cell_race_mut_immut_4() {
    loom::model(|| {
        let x = Data::new(1);
        let y = x.clone();
        let z = x.clone();

        let th1 = thread::spawn(move || x.get());
        let th2 = thread::spawn(move || assert_eq!(2, y.inc()));

        th1.join().unwrap();
        th2.join().unwrap();

        assert_eq!(3, z.inc());
    });
}

#[test]
#[should_panic]
fn unsafe_cell_race_mut_immut_5() {
    loom::model(|| {
        let x = Data::new(1);
        let y = x.clone();
        let z = x.clone();

        let th1 = thread::spawn(move || x.get());
        let th2 = thread::spawn(move || {
            assert_eq!(1, y.get());
            assert_eq!(2, y.inc());
        });

        th1.join().unwrap();
        th2.join().unwrap();

        assert_eq!(3, z.inc());
    });
}

#[test]
fn unsafe_cell_ok_1() {
    loom::model(|| {
        let x = Data::new(1);

        assert_eq!(2, x.inc());

        let th1 = thread::spawn(move || {
            assert_eq!(3, x.inc());
            x
        });

        let x = th1.join().unwrap();

        assert_eq!(4, x.inc());
    });
}

#[test]
fn unsafe_cell_ok_2() {
    loom::model(|| {
        let x = Data::new(1);

        assert_eq!(1, x.get());
        assert_eq!(2, x.inc());

        let th1 = thread::spawn(move || {
            assert_eq!(2, x.get());
            assert_eq!(3, x.inc());
            x
        });

        let x = th1.join().unwrap();

        assert_eq!(3, x.get());
        assert_eq!(4, x.inc());
    });
}

#[test]
fn unsafe_cell_ok_3() {
    loom::model(|| {
        let x = Data::new(1);
        let y = x.clone();

        let th1 = thread::spawn(move || {
            assert_eq!(1, x.get());

            let z = x.clone();
            let th2 = thread::spawn(move || {
                assert_eq!(1, z.get());
            });

            assert_eq!(1, x.get());
            th2.join().unwrap();
        });

        assert_eq!(1, y.get());

        th1.join().unwrap();

        assert_eq!(2, y.inc());
    });
}

#[test]
#[should_panic]
fn unsafe_cell_access_after_sync() {
    loom::model(|| {
        let s1 = Arc::new((AtomicUsize::new(0), UnsafeCell::new(0)));
        let s2 = s1.clone();

        thread::spawn(move || {
            s1.0.store(1, Release);
            s1.1.with_mut(|ptr| unsafe { *ptr = 1 });
        });

        if 1 == s2.0.load(Acquire) {
            s2.1.with_mut(|ptr| unsafe { *ptr = 2 });
        }
    });
}

/// Even when source locations are not being captured, the panic message names
/// the accesses (and threads) involved in the violation.
#[test]
#[should_panic(expected = "write two: thread #")]
fn unsafe_cell_race_lists_accesses_without_locations() {
    let mut builder = loom::model::Builder::new();
    builder.location = false;

    builder.check(|| {
        let x = Data::new(1);
        let y = Data(x.0.clone());

        let th1 = thread::spawn(move || x.inc());
        y.inc();

        th1.join().unwrap();
    });
}

#[test]
fn as_mut_is_ergonomical_and_safe() {
    loom::model(|| {
        let mut value = UnsafeCell::new(-3i32);
        let mut mut_ref = value.as_mut();
        *mut_ref = mut_ref.abs();
    });
}

/// `as_mut` is tracked like any other mutable access: it is an error while a
/// pointer handed out earlier is still alive.
#[test]
#[should_panic(expected = "currently reading from cell")]
fn as_mut_while_reading() {
    loom::model(|| {
        let mut value = UnsafeCell::new(1usize);
        let _reader = value.get();
        let _ = value.as_mut();
    });
}

#[test]
fn as_mut_sequential_accesses() {
    loom::model(|| {
        let mut value = UnsafeCell::new(vec![1usize]);

        value.as_mut().push(2);
        {
            let mut vec = value.as_mut();
            vec.push(3);
            assert_eq!(vec.len(), 3);
        }

        // The mutable access ended with the guard.
        value.with(|ptr| assert_eq!(unsafe { &*ptr }, &[1, 2, 3]));
        assert_eq!(value.into_inner(), [1, 2, 3]);
    });
}

/// Dropping the cell while a pointer obtained from it is still alive leaves
/// that pointer dangling.
#[test]
#[should_panic(expected = "dropped or consumed while immutable access")]
fn unsafe_cell_access_during_drop() {
    loom::model(|| {
        let x = UnsafeCell::new(());
        let _guard = x.get();
        drop(x);
    });
}

#[test]
#[should_panic(expected = "dropped or consumed while mutable access")]
fn unsafe_cell_mut_access_during_drop() {
    loom::model(|| {
        let x = UnsafeCell::new(1usize);
        let _guard = x.get_mut();
        drop(x);
    });
}

/// `into_inner` moves the value out from under any pointer just as well.
#[test]
#[should_panic(expected = "dropped or consumed while immutable access")]
fn unsafe_cell_access_during_into_inner() {
    loom::model(|| {
        let x = UnsafeCell::new(1usize);
        let _guard = x.get();
        let _ = x.into_inner();
    });
}

#[test]
fn unsafe_cell_into_inner_ok() {
    loom::model(|| {
        let x = UnsafeCell::new(0usize);
        {
            let _guard = x.get();
        }
        assert_eq!(x.into_inner(), 0);
    });
}

/// Unlike a `Drop` impl on `UnsafeCell<T>` itself, the check must not make
/// the mock stricter than `std::cell::UnsafeCell` under the drop checker:
/// values that borrow each other in a cycle through the cell still compile.
#[test]
fn unsafe_cell_drop_check_matches_std() {
    struct Node<'a> {
        next: UnsafeCell<Option<&'a Node<'a>>>,
    }

    loom::model(|| {
        let a = Node {
            next: UnsafeCell::new(None),
        };
        let b = Node {
            next: UnsafeCell::new(None),
        };

        a.next.with_mut(|next| unsafe { *next = Some(&b) });
        b.next.with_mut(|next| unsafe { *next = Some(&a) });

        assert!(a.next.with(|next| unsafe { (*next).is_some() }));
    });
}

/// A cell that outlives the model it was created in can still be dropped.
#[test]
fn unsafe_cell_dropped_after_model() {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    {
        let slot = slot.clone();
        loom::model(move || {
            slot.lock().unwrap().push(UnsafeCell::new(1usize));
        });
    }

    drop(slot);
}

/// A flag is published from inside a mutable access, which then continues.
/// A thread that sees the flag and reads the cell races with that access,
/// whatever else the writer does (or does not do) before the access ends.
fn publish_inside_mutable_access(extra_op: bool) {
    use loom::sync::atomic::AtomicBool;
    use loom::sync::atomic::Ordering::Relaxed;

    loom::model(move || {
        let s = Arc::new((UnsafeCell::new(0usize), AtomicBool::new(false)));
        let s2 = s.clone();

        let th = thread::spawn(move || {
            s2.0.with_mut(|p| {
                unsafe { *p = 1 };
                s2.1.store(true, Release);
                if extra_op {
                    s2.1.load(Relaxed);
                }
                unsafe { *p = 2 };
            });
        });

        if s.1.load(Acquire) {
            s.0.with(|p| unsafe { *p });
        }

        th.join().unwrap();
    });
}

#[test]
#[should_panic(expected = "Causality violation")]
fn race_with_open_mutable_access() {
    publish_inside_mutable_access(false);
}

#[test]
#[should_panic(expected = "Causality violation")]
fn race_with_open_mutable_access_extra_op() {
    publish_inside_mutable_access(true);
}

/// Same shape for a shared access raced by a writer.
#[test]
#[should_panic(expected = "Causality violation")]
fn race_with_open_shared_access() {
    use loom::sync::atomic::AtomicBool;

    loom::model(|| {
        let s = Arc::new((UnsafeCell::new(0usize), AtomicBool::new(false)));
        let s2 = s.clone();

        let th = thread::spawn(move || {
            s2.0.with(|p| {
                let _ = unsafe { *p };
                s2.1.store(true, Release);
                let _ = unsafe { *p };
            });
        });

        if s.1.load(Acquire) {
            s.0.with_mut(|p| unsafe { *p = 1 });
        }

        th.join().unwrap();
    });
}

/// Publishing after the access has ended is fine.
#[test]
fn publish_after_mutable_access() {
    use loom::sync::atomic::AtomicBool;

    loom::model(|| {
        let s = Arc::new((UnsafeCell::new(0usize), AtomicBool::new(false)));
        let s2 = s.clone();

        let th = thread::spawn(move || {
            s2.0.with_mut(|p| unsafe { *p = 1 });
            s2.1.store(true, Release);
        });

        if s.1.load(Acquire) {
            assert_eq!(s.0.with(|p| unsafe { *p }), 1);
        }

        th.join().unwrap();
    });
}
