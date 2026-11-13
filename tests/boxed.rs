#![deny(warnings, rust_2018_idioms)]

use loom::boxed::Box;
use loom::sync::atomic::{AtomicU8, Ordering};
use loom::sync::Arc;
use std::mem;

#[derive(Clone)]
struct DetectDrop(Arc<AtomicU8>);

impl DetectDrop {
    pub fn new() -> (Self, Arc<AtomicU8>) {
        let drop_count = Arc::new(AtomicU8::new(0));
        (Self(drop_count.clone()), drop_count)
    }
}

impl Drop for DetectDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn allocate_and_drop() {
    loom::model(|| {
        let (detect_drop, drop_count) = DetectDrop::new();
        let detect_drop = Box::new(detect_drop);
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);
        mem::drop(detect_drop);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn allocate_via_raw_and_drop() {
    loom::model(|| {
        let (detect_drop, drop_count) = DetectDrop::new();
        let detect_drop = Box::new(detect_drop);
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);
        let detect_drop_ptr: *mut DetectDrop = Box::into_raw(detect_drop);
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);
        let detect_drop = unsafe { Box::from_raw(detect_drop_ptr) };
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);
        mem::drop(detect_drop);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn into_inner() {
    loom::model(|| {
        let (detect_drop, drop_count) = DetectDrop::new();
        let detect_drop = Box::new(detect_drop);
        let value: DetectDrop = Box::into_inner(detect_drop);
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);
        mem::drop(value);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn clone() {
    loom::model(|| {
        let (detect_drop, drop_count) = DetectDrop::new();
        let detect_drop = Box::new(detect_drop);
        let detect_drop2: Box<DetectDrop> = detect_drop.clone();
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);
        mem::drop(detect_drop);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);
        mem::drop(detect_drop2);
        assert_eq!(drop_count.load(Ordering::SeqCst), 2);
    });
}

/// A box turned into a raw pointer and never turned back is a leak.
#[test]
#[should_panic(expected = "Allocation leaked")]
fn allocate_and_leak() {
    loom::model(|| {
        let _ = Box::into_raw(Box::new(1usize));
    });
}

/// The leaked box here also leaks the `Arc` it contains; whichever leak is
/// reported, it must be reported as a panic and not abort the process.
#[test]
#[should_panic(expected = "leaked")]
fn allocate_and_leak_contents() {
    loom::model(|| {
        let (detect_drop, _detect_drop) = DetectDrop::new();
        let detect_drop = Box::new(detect_drop);
        let _ = Box::into_raw(detect_drop);
    });
}

#[test]
fn same_size_as_std_box() {
    use std::boxed::Box as StdBox;

    macro_rules! same_size_and_alignment {
        ($t:ty) => {
            assert_eq!(
                mem::size_of::<Box<$t>>(),
                mem::size_of::<StdBox<$t>>(),
                "size of Box<{}>",
                stringify!($t),
            );
            assert_eq!(
                mem::align_of::<Box<$t>>(),
                mem::align_of::<StdBox<$t>>(),
                "align of Box<{}>",
                stringify!($t),
            );
            assert_eq!(
                mem::size_of::<Option<Box<$t>>>(),
                mem::size_of::<Option<StdBox<$t>>>(),
                "size of Option<Box<{}>>",
                stringify!($t),
            );
            assert_eq!(
                mem::align_of::<Option<Box<$t>>>(),
                mem::align_of::<Option<StdBox<$t>>>(),
                "align of Option<Box<{}>>",
                stringify!($t),
            );
        };
    }

    same_size_and_alignment!(std::convert::Infallible);
    same_size_and_alignment!(());
    same_size_and_alignment!(u8);
    same_size_and_alignment!([u32; 1024]);
}

/// Zero-sized values must not go through the allocator (a zero-sized
/// allocation is undefined behavior), and everything else still works.
#[test]
fn zero_sized() {
    struct Zst;

    loom::model(|| {
        let unit = Box::new(());
        assert_eq!(*unit, ());
        drop(unit);

        let raw = Box::into_raw(Box::new(Zst));
        let boxed = unsafe { Box::from_raw(raw) };
        let Zst = Box::into_inner(boxed);

        let _ = Box::into_inner(Box::new([0u64; 0]));
    });
}

/// Destructors of zero-sized values still run.
#[test]
fn zero_sized_drop() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct CountDrop;

    impl Drop for CountDrop {
        fn drop(&mut self) {
            DROPS.fetch_add(1, SeqCst);
        }
    }

    DROPS.store(0, SeqCst);

    loom::model(|| {
        let before = DROPS.load(SeqCst);
        drop(Box::new(CountDrop));
        assert_eq!(DROPS.load(SeqCst), before + 1);
    });
}

/// An unsized box, built from a raw allocation, is freed with the right
/// layout and runs the destructors of its contents.
#[test]
fn unsized_from_raw() {
    loom::model(|| {
        let (detect_drop, drop_count) = DetectDrop::new();

        let layout = std::alloc::Layout::new::<[DetectDrop; 2]>();
        let array = unsafe { loom::alloc::alloc(layout) } as *mut [DetectDrop; 2];
        unsafe { array.write([detect_drop.clone(), detect_drop]) };

        let slice: *mut [DetectDrop] = array;
        let boxed: Box<[DetectDrop]> = unsafe { Box::from_raw(slice) };
        assert_eq!(boxed.len(), 2);
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);

        drop(boxed);
        assert_eq!(drop_count.load(Ordering::SeqCst), 2);
    });
}

/// The usual use: ownership handed between threads as a raw pointer.
#[test]
fn raw_pointer_across_threads() {
    use loom::sync::atomic::AtomicPtr;

    loom::model(|| {
        let slot = Arc::new(AtomicPtr::new(std::ptr::null_mut::<usize>()));

        let th = {
            let slot = slot.clone();
            loom::thread::spawn(move || {
                slot.store(Box::into_raw(Box::new(7usize)), Ordering::Release);
            })
        };

        th.join().unwrap();

        let raw = slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
        let boxed = unsafe { Box::from_raw(raw) };
        assert_eq!(Box::into_inner(boxed), 7);
    });
}

#[test]
fn traits() {
    fn assert_send_sync<T: Send + Sync + Unpin>() {}
    assert_send_sync::<Box<usize>>();
    assert_send_sync::<Box<[usize]>>();

    loom::model(|| {
        let a: Box<usize> = Box::default();
        let b = Box::from(1usize);
        assert!(a < b);
        assert_eq!(format!("{} {:?}", a, b), "0 1");

        let pinned = Box::pin(5usize);
        assert_eq!(*pinned, 5);
    });
}
