#![deny(warnings, rust_2018_idioms)]

//! Loom objects are tied to the execution they were created in. Keeping one
//! around for a later execution is a bug in the model; these tests check that
//! it is reported as such rather than as an unrelated internal error.

use loom::cell::Cell;
use loom::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use loom::sync::Mutex;

// A `std` thread local (not `loom::thread_local!`) is not reset between
// executions, so the cell created in the first execution is still there in
// the second one.
thread_local! {
    static STALE_CELL: Cell<usize> = Cell::new(0);
}

#[test]
#[should_panic(expected = "does not belong to the current execution")]
fn cell_reused_across_executions() {
    loom::model(|| {
        let handle_a = loom::thread::spawn(|| {
            let _ = STALE_CELL.with(Cell::get);
        });
        let handle_b = loom::thread::spawn(|| ());
        handle_a.join().unwrap();
        handle_b.join().unwrap();
    });
}

#[test]
#[should_panic(expected = "does not belong to the current execution")]
fn atomic_reused_across_models() {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));

    {
        let slot = slot.clone();
        loom::model(move || {
            *slot.lock().unwrap() = Some(std::sync::Arc::new(AtomicUsize::new(1)));
        });
    }

    loom::model(move || {
        let stale = slot.lock().unwrap().clone().unwrap();
        stale.load(SeqCst);
    });
}

#[test]
#[should_panic(expected = "does not belong to the current execution")]
fn mutex_reused_across_models() {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));

    {
        let slot = slot.clone();
        loom::model(move || {
            *slot.lock().unwrap() = Some(std::sync::Arc::new(Mutex::new(1)));
        });
    }

    loom::model(move || {
        let stale = slot.lock().unwrap().clone().unwrap();
        let _guard = stale.lock().unwrap();
    });
}
