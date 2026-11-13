#![deny(warnings, rust_2018_idioms)]

use loom::alloc::{alloc, dealloc, Layout, Track};

#[test]
fn alloc_and_dealloc() {
    loom::model(|| {
        let layout = Layout::new::<u64>();
        unsafe {
            let ptr = alloc(layout);
            ptr.cast::<u64>().write(1);
            dealloc(ptr, layout);
        }
    });
}

/// A leaked raw allocation is reported with a panic. The leaked allocation's
/// tracker is torn down while that panic unwinds, which must not turn the
/// report into an abort.
#[test]
#[should_panic(expected = "Allocation leaked")]
fn leaked_alloc_is_reported() {
    loom::model(|| {
        let _ = unsafe { alloc(Layout::new::<u64>()) };
    });
}

#[test]
#[should_panic(expected = "Allocation leaked")]
fn leaked_track_is_reported() {
    loom::model(|| {
        std::mem::forget(Track::new(1u64));
    });
}

/// A tracked value that is (incorrectly) carried out of the model can still
/// be dropped afterwards.
#[test]
fn track_dropped_after_model() {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));

    {
        let slot = slot.clone();
        let result = std::panic::catch_unwind(move || {
            loom::model(move || {
                *slot.lock().unwrap() = Some(Track::new(1u64));
            });
        });

        // Keeping it alive past the execution counts as a leak.
        assert!(result.is_err());
    }

    drop(slot);
}
