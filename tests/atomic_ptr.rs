#![deny(warnings, rust_2018_idioms)]

use loom::sync::atomic::AtomicPtr;
use loom::sync::Arc;
use loom::thread;

use std::sync::atomic::Ordering::{Relaxed, SeqCst};

fn addr<T>(ptr: *mut T) -> usize {
    ptr as usize
}

#[test]
fn fetch_ptr_add_sub() {
    loom::model(|| {
        let mut array = [1u32, 2, 3, 4];
        let base = array.as_mut_ptr();
        let atomic = AtomicPtr::new(base);

        assert_eq!(atomic.fetch_ptr_add(2, SeqCst), base);
        let ptr = atomic.load(SeqCst);
        assert_eq!(ptr, base.wrapping_add(2));
        // The result keeps the provenance of the original pointer.
        assert_eq!(unsafe { *ptr }, 3);

        assert_eq!(atomic.fetch_ptr_sub(1, SeqCst), base.wrapping_add(2));
        assert_eq!(unsafe { *atomic.load(SeqCst) }, 2);
    });
}

#[test]
fn fetch_byte_add_sub() {
    loom::model(|| {
        let mut array = [1u32, 2, 3, 4];
        let base = array.as_mut_ptr();
        let atomic = AtomicPtr::new(base);

        assert_eq!(atomic.fetch_byte_add(4, SeqCst), base);
        let ptr = atomic.load(SeqCst);
        assert_eq!(addr(ptr), addr(base) + 4);
        assert_eq!(unsafe { *ptr }, 2);

        assert_eq!(atomic.fetch_byte_sub(4, SeqCst), ptr);
        assert_eq!(atomic.load(SeqCst), base);
    });
}

/// The classic use: tag bits in the low bits of an aligned pointer.
#[test]
fn fetch_bit_ops_tag_pointer() {
    loom::model(|| {
        let mut value = 7u64;
        let base: *mut u64 = &mut value;
        assert_eq!(addr(base) & 0b111, 0);

        let atomic = AtomicPtr::new(base);

        assert_eq!(atomic.fetch_or(0b01, SeqCst), base);
        assert_eq!(addr(atomic.load(SeqCst)), addr(base) | 0b01);

        assert_eq!(addr(atomic.fetch_xor(0b11, SeqCst)), addr(base) | 0b01);
        assert_eq!(addr(atomic.load(SeqCst)), addr(base) | 0b10);

        assert_eq!(addr(atomic.fetch_and(!0b111, SeqCst)), addr(base) | 0b10);
        let ptr = atomic.load(SeqCst);
        assert_eq!(ptr, base);
        assert_eq!(unsafe { *ptr }, 7);
    });
}

/// The operations are atomic read-modify-writes: concurrent updates are not
/// lost.
#[test]
fn fetch_or_concurrent() {
    loom::model(|| {
        let atomic = Arc::new(AtomicPtr::new(std::ptr::null_mut::<u64>()));

        let ths: Vec<_> = [0b01usize, 0b10]
            .into_iter()
            .map(|bit| {
                let atomic = atomic.clone();
                thread::spawn(move || {
                    atomic.fetch_or(bit, Relaxed);
                })
            })
            .collect();

        for th in ths {
            th.join().unwrap();
        }

        assert_eq!(addr(atomic.load(Relaxed)), 0b11);
    });
}

#[test]
fn fetch_ptr_add_concurrent() {
    loom::model(|| {
        let atomic = Arc::new(AtomicPtr::new(std::ptr::null_mut::<u32>()));

        let ths: Vec<_> = (0..2)
            .map(|_| {
                let atomic = atomic.clone();
                thread::spawn(move || {
                    atomic.fetch_ptr_add(1, Relaxed);
                })
            })
            .collect();

        for th in ths {
            th.join().unwrap();
        }

        assert_eq!(addr(atomic.load(Relaxed)), 2 * std::mem::size_of::<u32>());
    });
}
