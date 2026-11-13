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

/// Freeing the same allocation twice is reported as such (and the second
/// free does not reach the real allocator).
#[test]
#[should_panic(expected = "Double free")]
fn double_free_is_reported() {
    loom::model(|| {
        let layout = Layout::new::<u64>();
        unsafe {
            let ptr = alloc(layout);
            dealloc(ptr, layout);
            dealloc(ptr, layout);
        }
    });
}

/// A double free by two threads that both think they own the allocation.
#[test]
#[should_panic(expected = "Double free")]
fn double_free_across_threads_is_reported() {
    use loom::sync::atomic::{AtomicPtr, Ordering::SeqCst};
    use loom::sync::Arc;

    loom::model(|| {
        let layout = Layout::new::<u64>();
        let slot = Arc::new(AtomicPtr::new(unsafe { alloc(layout) }));

        let th = {
            let slot = slot.clone();
            loom::thread::spawn(move || {
                // Bug: reads the pointer without taking it out of the slot.
                unsafe { dealloc(slot.load(SeqCst), layout) };
            })
        };

        unsafe { dealloc(slot.load(SeqCst), layout) };
        th.join().unwrap();
    });
}

/// Freeing something loom never saw being allocated.
#[test]
#[should_panic(expected = "not tracking")]
fn free_of_untracked_pointer_is_reported() {
    loom::model(|| {
        let layout = Layout::new::<u64>();
        unsafe {
            let ptr = std::alloc::alloc(layout);
            // Leaks the real allocation; the model panics before freeing it.
            dealloc(ptr, layout);
        }
    });
}

/// The allocator reusing a freed address is not a double free.
#[test]
fn address_reuse_is_fine() {
    loom::model(|| {
        let layout = Layout::new::<u64>();
        for _ in 0..8 {
            unsafe {
                let ptr = alloc(layout);
                dealloc(ptr, layout);
            }
        }
    });
}
