#![deny(warnings, rust_2018_idioms)]

//! `AtomicPtr::load_consume`: only what is done with the pointer comes after
//! the store that is read.

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicPtr, AtomicUsize};
use loom::sync::Arc;
use loom::thread;

use std::ptr;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

struct Node {
    cell: UnsafeCell<usize>,
    atomic: AtomicUsize,
    next: AtomicPtr<Node>,
}

impl Node {
    fn new() -> *mut Node {
        Box::into_raw(Box::new(Node {
            cell: UnsafeCell::new(0),
            atomic: AtomicUsize::new(0),
            next: AtomicPtr::new(ptr::null_mut()),
        }))
    }
}

/// Publishes a node, written to after it was made, and reads it in another
/// thread with `load`. Frees the node.
fn publish(release: std::sync::atomic::Ordering, load: fn(&AtomicPtr<Node>) -> *mut Node) {
    let head = Arc::new(AtomicPtr::new(ptr::null_mut()));
    let node = Node::new();

    let th = {
        let head = head.clone();
        thread::spawn(move || {
            let node = load(&head);
            if !node.is_null() {
                let node = unsafe { &*node };
                assert_eq!(node.cell.with(|v| unsafe { *v }), 1);
                // The store of 0 is the creation of the atomic, which the
                // store of 1 comes after.
                assert_eq!(node.atomic.load(Relaxed), 1);
            }
        })
    };

    unsafe {
        (*node).cell.with_mut(|v| *v = 1);
        (*node).atomic.store(1, Relaxed);
    }
    head.store(node, release);

    th.join().unwrap();
    drop(unsafe { Box::from_raw(node) });
}

#[test]
fn dependent_accesses_come_after_the_store() {
    loom::model(|| publish(Release, |head| head.load_consume()));
}

#[test]
#[should_panic(expected = "Causality violation")]
fn relaxed_load_orders_nothing() {
    loom::model(|| publish(Release, |head| head.load(Relaxed)));
}

/// A consume load reads a store that releases nothing as a `Relaxed` load
/// does.
#[test]
#[should_panic(expected = "Causality violation")]
fn needs_a_release_store() {
    loom::model(|| publish(Relaxed, |head| head.load_consume()));
}

/// What is not reached through the pointer is not ordered: here `Acquire`
/// is needed, and a compiler that makes an `Acquire` load of a consume load
/// hides that.
fn independent(load: fn(&AtomicPtr<usize>) -> *mut usize) {
    let head = Arc::new(AtomicPtr::new(ptr::null_mut()));
    let other = Arc::new(UnsafeCell::new(0));
    let value = Box::into_raw(Box::new(0));

    let th = {
        let head = head.clone();
        let other = other.clone();
        thread::spawn(move || {
            if !load(&head).is_null() {
                other.with(|v| unsafe { *v });
            }
        })
    };

    other.with_mut(|v| unsafe { *v = 1 });
    head.store(value, Release);

    th.join().unwrap();
    drop(unsafe { Box::from_raw(value) });
}

#[test]
fn acquire_orders_independent_accesses() {
    loom::model(|| independent(|head| head.load(Acquire)));
}

#[test]
#[should_panic(expected = "Causality violation")]
fn consume_does_not_order_independent_accesses() {
    loom::model(|| independent(|head| head.load_consume()));
}

/// A dependency is carried through a second consume load, of a pointer that
/// is in the first pointee.
#[test]
fn dependency_follows_a_chain() {
    loom::model(|| {
        let head = Arc::new(AtomicPtr::new(ptr::null_mut::<Node>()));
        let first = Node::new();
        let second = Node::new();

        let th = {
            let head = head.clone();
            thread::spawn(move || {
                let first = head.load_consume();
                if !first.is_null() {
                    let second = unsafe { &*first }.next.load_consume();
                    assert!(!second.is_null());
                    assert_eq!(unsafe { &*second }.cell.with(|v| unsafe { *v }), 2);
                }
            })
        };

        unsafe {
            (*second).cell.with_mut(|v| *v = 2);
            // Nothing is released here: the store of `first` is what the
            // other thread gets to `second` by.
            (*first).next.store(second, Relaxed);
        }
        head.store(first, Release);

        th.join().unwrap();
        unsafe {
            drop(Box::from_raw(first));
            drop(Box::from_raw(second));
        }
    });
}

/// The load returns what a `Relaxed` load does: either value, whatever the
/// thread goes on to do.
#[test]
fn reads_any_store_a_relaxed_load_does() {
    use std::sync::atomic::AtomicUsize as StdAtomicUsize;
    use std::sync::atomic::Ordering::SeqCst;

    let seen = std::sync::Arc::new(StdAtomicUsize::new(0));
    let seen2 = seen.clone();

    loom::model(move || {
        let head = Arc::new(AtomicPtr::new(ptr::null_mut::<usize>()));
        let value = Box::into_raw(Box::new(0usize));

        let th = {
            let head = head.clone();
            thread::spawn(move || head.load_consume().is_null())
        };

        head.store(value, Release);

        let null = th.join().unwrap();
        seen2.fetch_or(if null { 1 } else { 2 }, SeqCst);
        drop(unsafe { Box::from_raw(value) });
    });

    assert_eq!(seen.load(SeqCst), 3);
}

/// What is behind a second pointer in the pointee, and was made before the
/// store, depends on the load too.
#[test]
fn dependency_follows_a_box() {
    struct Outer {
        inner: Box<Node>,
    }

    loom::model(|| {
        let head = Arc::new(AtomicPtr::new(ptr::null_mut::<Outer>()));

        let th = {
            let head = head.clone();
            thread::spawn(move || {
                let outer = head.load_consume();
                if !outer.is_null() {
                    let inner = &unsafe { &*outer }.inner;
                    assert_eq!(inner.cell.with(|v| unsafe { *v }), 1);
                    assert_eq!(inner.atomic.load(Relaxed), 1);
                }
            })
        };

        let inner = unsafe { Box::from_raw(Node::new()) };
        inner.cell.with_mut(|v| unsafe { *v = 1 });
        inner.atomic.store(1, Relaxed);
        let outer = Box::into_raw(Box::new(Outer { inner }));
        head.store(outer, Release);

        th.join().unwrap();
        drop(unsafe { Box::from_raw(outer) });
    });
}

/// Only the access that is made with the pointer comes after the store, and
/// not what the thread does next.
#[test]
#[should_panic(expected = "Causality violation")]
fn dependent_access_does_not_order_what_follows() {
    loom::model(|| {
        let head = Arc::new(AtomicPtr::new(ptr::null_mut::<Node>()));
        let other = Arc::new(UnsafeCell::new(0));
        let node = Node::new();

        let th = {
            let head = head.clone();
            let other = other.clone();
            thread::spawn(move || {
                let node = head.load_consume();
                if !node.is_null() {
                    let node = unsafe { &*node };
                    assert_eq!(node.cell.with(|v| unsafe { *v }), 1);
                    assert_eq!(node.atomic.load(Relaxed), 1);
                    // Not got to through the pointer.
                    other.with(|v| unsafe { *v });
                }
            })
        };

        other.with_mut(|v| unsafe { *v = 1 });
        unsafe {
            (*node).cell.with_mut(|v| *v = 1);
            (*node).atomic.store(1, Relaxed);
        }
        head.store(node, Release);

        th.join().unwrap();
        drop(unsafe { Box::from_raw(node) });
    });
}

/// What a dependent access acquires by itself, the thread keeps: without
/// the `Acquire` load, this is `dependent_access_does_not_order_what_follows`.
#[test]
fn dependent_acquire_load_is_kept() {
    loom::model(|| {
        let head = Arc::new(AtomicPtr::new(ptr::null_mut::<Node>()));
        let other = Arc::new(UnsafeCell::new(0));
        let node = Node::new();

        let th = {
            let head = head.clone();
            let other = other.clone();
            thread::spawn(move || {
                let node = head.load_consume();
                if !node.is_null() {
                    // The store of 1 comes before the one that was consumed.
                    assert_eq!(unsafe { &*node }.atomic.load(Acquire), 1);
                    other.with(|v| unsafe { *v });
                }
            })
        };

        other.with_mut(|v| unsafe { *v = 1 });
        unsafe { (*node).atomic.store(1, Release) };
        head.store(node, Release);

        th.join().unwrap();
        drop(unsafe { Box::from_raw(node) });
    });
}

/// A thread reads its own dependent store back, and what it stores next
/// comes after it.
#[test]
fn dependent_store_is_the_threads_own() {
    loom::model(|| {
        let head = Arc::new(AtomicPtr::new(ptr::null_mut::<Node>()));
        let node = Node::new();

        let th = {
            let head = head.clone();
            thread::spawn(move || {
                let node = head.load_consume();
                if !node.is_null() {
                    let node = unsafe { &*node };
                    node.atomic.store(2, Relaxed);
                    assert_eq!(node.atomic.load(Relaxed), 2);
                    node.atomic.store(3, Relaxed);
                    assert_eq!(node.atomic.load(Relaxed), 3);
                }
            })
        };

        unsafe { (*node).atomic.store(1, Relaxed) };
        head.store(node, Release);

        th.join().unwrap();
        drop(unsafe { Box::from_raw(node) });
    });
}

/// The pointer may be anything, such as a marker at the end of the address
/// space, that nothing is accessed through.
#[test]
fn pointer_at_the_end_of_the_address_space() {
    loom::model(|| {
        let marker = usize::MAX as *mut [u8; 64];
        let head = AtomicPtr::new(marker);

        assert_eq!(head.load_consume(), marker);
        // The second load finds what the first one left.
        assert_eq!(head.load_consume(), marker);
    });
}

/// An unsynchronized load is a load: it does not order what follows either.
#[test]
#[should_panic(expected = "Concurrent read and write accesses")]
fn dependent_unsync_load_does_not_order_what_follows() {
    loom::model(|| {
        let head = Arc::new(AtomicPtr::new(ptr::null_mut::<Node>()));
        let other = Arc::new(UnsafeCell::new(0));
        let node = Node::new();

        let th = {
            let head = head.clone();
            let other = other.clone();
            thread::spawn(move || {
                let node = head.load_consume();
                if !node.is_null() {
                    // The writer is done with the node: this is no race.
                    assert_eq!(unsafe { (*node).atomic.unsync_load() }, 1);
                    // Not got to through the pointer.
                    other.with(|v| unsafe { *v });
                }
            })
        };

        other.with_mut(|v| unsafe { *v = 1 });
        unsafe { (*node).atomic.store(1, Relaxed) };
        head.store(node, Release);

        th.join().unwrap();
        drop(unsafe { Box::from_raw(node) });
    });
}
