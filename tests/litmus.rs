#![deny(warnings, rust_2018_idioms)]

use loom::sync::atomic::AtomicUsize;
use loom::thread;

use std::collections::HashSet;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

/// The main thread's load can read the 1 that the other thread copies from
/// the main thread's later store. RC11, which loom follows by default, rules
/// that out; the C++ standard allows it, and `Builder::load_buffering`
/// explores it.
#[test]
fn load_buffering() {
    let values = Arc::new(Mutex::new(HashSet::new()));
    let values_ = values.clone();
    let mut builder = loom::model::Builder::new();
    builder.load_buffering = true;
    builder.check(move || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let th = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(y.load(Relaxed), Relaxed);
            })
        };

        let a = x.load(Relaxed);
        y.store(1, Relaxed);

        th.join().unwrap();

        // Not for an execution in which the load's guess did not come true.
        if loom::model::is_settled() {
            values.lock().unwrap().insert(a);
        }
    });
    assert!(values_.lock().unwrap().contains(&1));
}

#[test]
fn store_buffering() {
    let values = Arc::new(Mutex::new(HashSet::new()));
    let values_ = values.clone();
    loom::model(move || {
        let x = Arc::new(AtomicUsize::new(0));
        let y = Arc::new(AtomicUsize::new(0));

        let a = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                x.store(1, Relaxed);
                y.load(Relaxed)
            })
        };

        y.store(1, Relaxed);
        let b = x.load(Relaxed);

        let a = a.join().unwrap();
        values.lock().unwrap().insert((a, b));
    });
    assert!(values_.lock().unwrap().contains(&(0, 0)));
}
